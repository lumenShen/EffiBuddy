//! Persisted session state.
//!
//! A session is the single source of truth for the UI and the harness. The UI
//! reads it on every frame; the harness mutates it inside short critical
//! sections and emits fine-grained events for things the UI must react to
//! immediately (stream deltas, approval requests, run lifecycle).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::checkpoint::TurnCheckpoint;
use crate::ids::new_id;
use crate::message::{Message, Role, TodoItem, ToolResult, Usage, truncate_chars};
use crate::network::AgentGraphRecord;
use crate::tool::PermissionMode;

pub const DEFAULT_SESSION_TITLE: &str = "";

/// Shared handle used by the harness, tool implementations and the UI.
pub type SharedSession = Arc<Mutex<SessionState>>;

pub fn lock_session(session: &SharedSession) -> MutexGuard<'_, SessionState> {
    session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn new_shared_session(session: SessionState) -> SharedSession {
    Arc::new(Mutex::new(session))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
    WaitingApproval,
}

impl RunStatus {
    pub fn is_busy(self) -> bool {
        matches!(self, RunStatus::Running | RunStatus::WaitingApproval)
    }
}

/// Why a compaction ran. Kept in the record so the UI and transcripts can
/// distinguish proactive threshold compaction from the rescue path used after
/// a provider rejected an oversized request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    #[default]
    Auto,
    Reactive,
}

/// Where the summary text came from. Model summaries are the normal path;
/// `Heuristic` marks the offline fallback used when the summary request itself
/// fails, so a later reader does not mistake it for a full-fidelity summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummarySource {
    #[default]
    Heuristic,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    /// Index in `messages` up to which the summary replaces the transcript.
    pub compacted_messages: usize,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub trigger: CompactionTrigger,
    #[serde(default)]
    pub summary_source: SummarySource,
    /// Estimated request tokens immediately before and after this compaction.
    #[serde(default)]
    pub pre_tokens: usize,
    #[serde(default)]
    pub post_tokens: usize,
    /// Messages covered by this summary and messages kept verbatim.
    #[serde(default)]
    pub summarized_messages: usize,
    #[serde(default)]
    pub kept_messages: usize,
    /// Token threshold that was in force when this compaction ran.
    #[serde(default)]
    pub auto_threshold: usize,
}

impl CompactionRecord {
    pub fn source_label(&self) -> &'static str {
        match self.summary_source {
            SummarySource::Model => "model",
            SummarySource::Heuristic => "heuristic",
        }
    }
}

/// State of a summary that is being written ahead of the compaction threshold
/// (OpenCode's "background compaction").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeldCompactionStatus {
    #[default]
    Writing,
    Ready,
    Failed,
}

/// A summary generated from an earlier transcript snapshot. It is held in
/// memory without touching the request window until compaction is due, then
/// swapped in if the snapshot is still valid. Never serialized: it is derived
/// work, and a restart simply starts a new summary.
#[derive(Debug, Clone)]
pub struct HeldCompaction {
    /// Runtime id used to discard results from a superseded task.
    pub id: String,
    pub status: HeldCompactionStatus,
    pub summary: Option<String>,
    pub start: usize,
    pub boundary: usize,
    pub boundary_id: String,
    pub summarized_messages: usize,
    pub kept_messages: usize,
    /// Request estimate when the snapshot was taken, used to detect a summary
    /// that has become too stale to swap in.
    pub pre_tokens: usize,
    pub model: String,
    pub provider_id: Option<String>,
    pub started_at: DateTime<Utc>,
}

impl HeldCompaction {
    pub fn is_ready(&self) -> bool {
        self.status == HeldCompactionStatus::Ready
            && self
                .summary
                .as_deref()
                .is_some_and(|summary| !summary.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Interrupted,
    Failed(String),
}

impl RunOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Interrupted => "interrupted",
            RunOutcome::Failed(_) => "failed",
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, RunOutcome::Completed)
    }
}

/// Cross-restart terminal record written into a subagent session file when its
/// run reaches a terminal state. This is the only durable source of "how did
/// this child end"; a restored child session without it is
/// [`crate::SubagentStatus::Lost`]. Mirrors ZCode's decision to keep a single
/// metadata record instead of inferring status from transcript tails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildRunRecord {
    /// Runtime id of the agent object; the graph overlays by this key.
    pub agent_id: String,
    pub profile: String,
    pub color: crate::subagent::SubagentColor,
    pub status: crate::subagent::SubagentStatus,
    pub depth: usize,
    pub ended_at: DateTime<Utc>,
    #[serde(default)]
    pub usage: Usage,
}

/// Everything needed to restore a conversation from disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub cwd: PathBuf,
    #[serde(default = "default_model")]
    pub model: String,
    /// Registry provider that serves `model`. Old transcripts have `None` and
    /// are resolved by model id when opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Optional reasoning effort (`low`/`medium`/`high`) for models that expose
    /// selectable levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub usage: Usage,
    /// Rolling summary of messages that were compacted out of the request.
    #[serde(default)]
    pub compaction: Option<CompactionRecord>,
    /// Index into `messages`; everything before it is represented by the
    /// summary instead of being sent to the model.
    #[serde(default)]
    pub compacted_upto: usize,
    /// Tool call ids whose result body is replaced with a short placeholder on
    /// the wire. The transcript keeps the original output for the UI; only the
    /// next request is shrunk. This is ZCode's microcompact stage, run before a
    /// full model summary when tool output dominates the context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleared_tool_calls: Vec<String>,
    /// Background-compaction snapshot. Memory only: a held summary is derived
    /// work and is silently dropped on restart, matching OpenCode.
    #[serde(skip)]
    pub held_compaction: Option<HeldCompaction>,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub steps: usize,
    #[serde(default)]
    pub pinned: bool,
    /// UI hint set from the sidebar's “标为未读” action and cleared when the
    /// conversation is opened. Never sent to the model or counted as work.
    #[serde(default)]
    pub unread: bool,
    /// Archived conversations leave the sidebar and only appear in
    /// Settings → 归档. Archiving marks the conversation as finished work.
    #[serde(default)]
    pub archived: bool,
    /// Workspace this conversation was started from when the user opened it
    /// inside a project. `None` puts the conversation in the sidebar's 任务
    /// section. The field is always serialized (`null` for tasks) so old
    /// transcripts without the key can be migrated to their `cwd` project.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Stable id of the project (see `projects.json`). New conversations set
    /// this together with `project`; legacy transcripts only have the path and
    /// are grouped by resolving it against the project roots.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Global permission mode for this conversation (`default`, `accept_edits`,
    /// `plan`, `full_access`). Cycled with Shift+Tab in the UI; the approval
    /// gate reads it on every tool call. Old transcripts default to
    /// [`PermissionMode::Default`].
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// Set on sessions that belong to a subagent. Parent sessions have `None`
    /// and only those appear in the sidebar.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Agent profile name for subagent sessions.
    #[serde(default)]
    pub agent_name: Option<String>,
    /// Short task description shown in the agent panel.
    #[serde(default)]
    pub agent_description: Option<String>,
    /// Terminal record for subagent sessions, stamped when the run ends.
    /// `None` on parent sessions and on children that never finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_run: Option<ChildRunRecord>,
    /// Persisted agent relationship graph for this session: nodes, edges,
    /// inter-agent messages and the viewport (pan/zoom). Live runtime state is
    /// projected over this record by the UI.
    #[serde(default)]
    pub graph: AgentGraphRecord,
    /// Original file state captured before each tool edit, grouped by the user
    /// message whose turn produced the edits. Used by the message edit/undo
    /// flow to restore the workspace without touching Git.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<TurnCheckpoint>,
    /// Runtime `message_id → index into messages` map. `messages` is
    /// append-only (compaction moves `compacted_upto`, never removes), so the
    /// map stays valid without rebuilds; streamed deltas use it for O(1)
    /// lookups instead of a linear scan per token. Derived state — rebuilt on
    /// load, never persisted.
    #[serde(skip)]
    pub message_index: HashMap<String, usize>,
}

fn default_model() -> String {
    "deepseek-chat".to_string()
}

impl SessionState {
    pub fn new(cwd: PathBuf, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: new_id(),
            title: DEFAULT_SESSION_TITLE.to_string(),
            cwd,
            model: model.into(),
            provider_id: None,
            reasoning_level: None,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            todos: Vec::new(),
            usage: Usage::default(),
            compaction: None,
            compacted_upto: 0,
            cleared_tool_calls: Vec::new(),
            held_compaction: None,
            status: RunStatus::Idle,
            last_error: None,
            steps: 0,
            pinned: false,
            unread: false,
            archived: false,
            project: None,
            project_id: None,
            permission_mode: PermissionMode::default(),
            parent_id: None,
            agent_name: None,
            agent_description: None,
            child_run: None,
            graph: AgentGraphRecord::default(),
            checkpoints: Vec::new(),
            message_index: HashMap::new(),
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// Switch the model for this conversation. The harness is rebuilt from the
    /// session on the next turn, so this never mutates a running harness.
    pub fn set_model_selection(
        &mut self,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        reasoning_level: Option<String>,
    ) {
        let provider_id = provider_id.into();
        let model = model.into();
        let changed =
            self.provider_id.as_deref() != Some(provider_id.as_str()) || self.model != model;
        self.provider_id = Some(provider_id);
        self.model = model;
        self.reasoning_level = reasoning_level;
        if changed {
            // The reported footprint belongs to the previous model's context
            // window; keeping it would make the next compaction decision
            // compare tokens from one tokenizer against another model's window.
            self.usage.last_prompt_tokens = 0;
            // A summary written by the previous model cannot be swapped into a
            // request served by a different model.
            self.held_compaction = None;
        }
        self.touch();
    }

    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            "新的会话".to_string()
        } else {
            self.title.clone()
        }
    }

    pub fn push_message(&mut self, message: Message) {
        if message.role == Role::User {
            let text = message.text();
            if text.trim().is_empty() {
                // An attachment-only first message still deserves a title.
                if let Some(attachment) = message.attachments.first() {
                    self.maybe_set_title(&format!("附件 · {}", attachment.name));
                }
            } else {
                self.maybe_set_title(&text);
            }
        }
        self.message_index
            .insert(message.id.clone(), self.messages.len());
        self.messages.push(message);
        self.touch();
    }

    /// O(1) message lookup by id. Falls back to a scan miss when the id is
    /// stale (the index is rebuilt on load and maintained on push).
    pub fn message_by_id(&mut self, message_id: &str) -> Option<&mut Message> {
        let index = *self.message_index.get(message_id)?;
        let message = self.messages.get_mut(index)?;
        (message.id == message_id).then_some(message)
    }

    /// Index of a message, without mutating the session.
    pub fn message_position(&self, message_id: &str) -> Option<usize> {
        self.messages
            .iter()
            .position(|message| message.id == message_id)
    }

    /// Id of the visible user message that owns the current turn. Synthetic
    /// notifications, inter-agent messages and mid-turn steers are skipped so
    /// a background wake keeps appending to the turn the user actually typed.
    pub fn latest_user_turn_id(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|message| {
                message.role == Role::User
                    && !message.synthetic
                    && !message.mid_turn
                    && message.agent_message.is_none()
            })
            .map(|message| message.id.as_str())
    }

    pub fn checkpoint(&self, message_id: &str) -> Option<&TurnCheckpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.message_id == message_id)
    }

    pub fn checkpoint_mut(&mut self, message_id: &str) -> Option<&mut TurnCheckpoint> {
        self.checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.message_id == message_id)
    }

    /// Borrow the checkpoint for `message_id`, creating it on first use.
    pub fn checkpoint_or_insert(&mut self, message_id: &str) -> &mut TurnCheckpoint {
        if let Some(index) = self
            .checkpoints
            .iter()
            .position(|checkpoint| checkpoint.message_id == message_id)
        {
            return &mut self.checkpoints[index];
        }
        self.checkpoints
            .push(TurnCheckpoint::new(message_id.to_string()));
        self.checkpoints
            .last_mut()
            .expect("checkpoint was just pushed")
    }

    pub fn remove_checkpoint(&mut self, message_id: &str) -> Option<TurnCheckpoint> {
        let index = self
            .checkpoints
            .iter()
            .position(|checkpoint| checkpoint.message_id == message_id)?;
        Some(self.checkpoints.remove(index))
    }

    /// Visible user turn ids at or after `index`. Used by the rewind flow to
    /// decide which checkpoints become unreachable when the transcript is
    /// trimmed.
    pub fn user_turn_ids_from(&self, index: usize) -> Vec<String> {
        self.messages
            .iter()
            .skip(index)
            .filter(|message| {
                message.role == Role::User && !message.synthetic && message.agent_message.is_none()
            })
            .map(|message| message.id.clone())
            .collect()
    }

    /// Remove every message from `index` onward. This is the transcript half
    /// of edit/undo; the caller restores files and drops checkpoint metadata
    /// around it. Compaction that covered a removed prefix is discarded so the
    /// next request cannot carry a summary of messages that no longer exist.
    pub fn truncate_from(&mut self, index: usize) -> Vec<Message> {
        if index >= self.messages.len() {
            return Vec::new();
        }
        // The sidebar title is derived from the first visible user message.
        // Dropping that message must clear the title so an edited first prompt
        // can become the new title instead of leaving the deleted one behind.
        let title_source_removed = !self.messages[..index].iter().any(|message| {
            message.role == Role::User && !message.synthetic && message.agent_message.is_none()
        });
        let removed = self.messages.split_off(index);
        if title_source_removed {
            self.title.clear();
        }
        if self.compacted_upto > index {
            self.compacted_upto = 0;
            self.compaction = None;
        }
        // The snapshot a held summary was built from no longer exists.
        self.held_compaction = None;
        self.steps = 0;
        // The current context footprint no longer describes this transcript.
        // Cumulative billing counters stay untouched; only the context-ring
        // value resets until the next model request reports a fresh one.
        self.usage.last_prompt_tokens = 0;
        // Cleared ids for messages that no longer exist can never match a
        // future tool call; drop them instead of letting the list grow.
        self.cleared_tool_calls.retain(|call_id| {
            self.messages[..index]
                .iter()
                .any(|message| message.tool_result().is_some_and(|r| &r.call_id == call_id))
        });
        self.rebuild_message_index();
        self.touch();
        removed
    }

    fn maybe_set_title(&mut self, text: &str) {
        if !self.title.trim().is_empty() {
            return;
        }
        let first_line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        if first_line.is_empty() {
            return;
        }
        self.title = truncate_chars(first_line, 40);
    }

    pub fn set_status(&mut self, status: RunStatus) {
        self.status = status;
        self.touch();
    }

    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.text())
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last()
    }

    /// The window of messages sent to the provider after context compaction.
    pub fn visible_messages(&self) -> &[Message] {
        let start = self.compacted_upto.min(self.messages.len());
        &self.messages[start..]
    }

    pub fn total_messages(&self) -> usize {
        self.messages.len()
    }

    pub fn apply_todos(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
        self.touch();
    }

    pub fn add_usage(&mut self, usage: &Usage) {
        self.usage.add(usage);
        self.touch();
    }

    /// A session file can be saved while a stream is in flight or a process is
    /// killed. On load we normalize transient state and repair transcripts that
    /// end with tool calls but no tool results, because providers reject that
    /// shape on the next request.
    pub fn normalize_after_load(&mut self) {
        self.status = RunStatus::Idle;
        self.updated_at = Utc::now();
        // Held summaries are in-memory only; a loaded session starts clean.
        self.held_compaction = None;
        self.repair_dangling_tool_calls();
        self.rebuild_message_index();
        self.cleared_tool_calls.retain(|call_id| {
            self.messages
                .iter()
                .any(|message| message.tool_result().is_some_and(|r| &r.call_id == call_id))
        });
    }

    fn rebuild_message_index(&mut self) {
        self.message_index.clear();
        for (index, message) in self.messages.iter().enumerate() {
            self.message_index.insert(message.id.clone(), index);
        }
    }

    fn repair_dangling_tool_calls(&mut self) {
        let mut calls: Vec<crate::message::ToolCall> = Vec::new();
        for message in &self.messages {
            match message.role {
                Role::Assistant => {
                    calls.extend(message.tool_calls());
                }
                Role::Tool => {
                    if let Some(result) = message.tool_result() {
                        calls.retain(|call| call.id != result.call_id);
                    }
                }
                Role::User => calls.clear(),
                Role::System => {}
            }
        }
        for call in calls {
            let result =
                ToolResult::failure(&call, "Tool execution was interrupted before completion.");
            let message = Message::from_tool_result(result);
            self.message_index
                .insert(message.id.clone(), self.messages.len());
            self.messages.push(message);
        }
        if self.compacted_upto > self.messages.len() {
            self.compacted_upto = 0;
            self.compaction = None;
        }
    }

    pub fn last_assistant_message_id(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_session() -> SessionState {
        let mut session = SessionState::new(PathBuf::from("/tmp/workspace"), "deepseek-chat");
        session.push_message(Message::user("hello"));
        session
    }

    #[test]
    fn permission_mode_roundtrips_snake_case() {
        for (mode, expected_json) in [
            (PermissionMode::Default, "\"default\""),
            (PermissionMode::AcceptEdits, "\"accept_edits\""),
            (PermissionMode::Plan, "\"plan\""),
            (PermissionMode::FullAccess, "\"full_access\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), expected_json);
        }

        let mut session = sample_session();
        session.permission_mode = PermissionMode::AcceptEdits;
        let text = serde_json::to_string(&session).unwrap();
        assert!(text.contains("\"permission_mode\":\"accept_edits\""));
        let restored: SessionState = serde_json::from_str(&text).unwrap();
        assert_eq!(restored.permission_mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn legacy_session_without_permission_mode_defaults_to_ask() {
        let session = sample_session();
        let mut value = serde_json::to_value(&session).unwrap();
        value
            .as_object_mut()
            .expect("session serializes to an object")
            .remove("permission_mode");
        let restored: SessionState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.permission_mode, PermissionMode::Default);
        assert_eq!(restored.messages.len(), session.messages.len());
    }

    #[test]
    fn unknown_permission_mode_is_rejected() {
        let mut session = sample_session();
        session.permission_mode = PermissionMode::Plan;
        let mut value = serde_json::to_value(&session).unwrap();
        value
            .as_object_mut()
            .expect("session serializes to an object")
            .insert("permission_mode".to_string(), serde_json::json!("yolo"));
        assert!(serde_json::from_value::<SessionState>(value).is_err());
    }

    #[test]
    fn truncating_the_first_user_message_clears_the_title() {
        let mut session = sample_session();
        assert_eq!(session.title, "hello");
        session.push_message(Message::assistant());
        session.truncate_from(0);
        assert!(session.messages.is_empty());
        assert!(session.title.is_empty());
    }
}
