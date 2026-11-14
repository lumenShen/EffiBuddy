//! Subagent profiles and the spawner boundary.
//!
//! A subagent is a second [`crate::AgentHarness`] with its own transcript,
//! tool registry and cancellation token. The parent model only sees a compact
//! report; the child transcript stays available to the UI.
//!
//! This module deliberately contains **no runtime implementation**. The
//! desktop app supplies a [`SubagentSpawner`] (it owns the provider, the
//! session store and the event channel), while the `agent` tool in
//! `effibuddy-agent-tools` depends only on this boundary.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::message::Usage;
use crate::network::{AgentDescriptor, AgentMessageKind, AgentSendOutcome, AgentVisibility};
use crate::tool::ApprovalDecision;

/// Absolute recursion limit for agent trees. A direct child of the main agent
/// has depth 1; the deepest allowed agent has depth 3. The host enforces this
/// independently of prompt text and tool arguments.
pub const MAX_AGENT_DEPTH: usize = 3;

/// Accent color used by the UI for an agent type. Mirrors the idea that each
/// profile has a stable visual identity instead of a user-chosen color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentColor {
    Blue,
    Green,
    Purple,
    Amber,
}

/// A named agent type. Built-ins are defined in code; Markdown files under
/// `.effibuddy/agents/*.md` (user and project scope) extend and override the
/// catalog by name. See [`crate::profile_files`].
#[derive(Clone, Debug)]
pub struct SubagentProfile {
    pub name: String,
    pub description: String,
    /// System prompt body for the child harness.
    pub prompt: String,
    /// Tool names the child may use. Empty means "inherit the parent set,
    /// minus the `agent` tool" (no recursive subagents).
    pub tools: Vec<String>,
    /// Blacklist applied after [`Self::tools`]. Names here never reach the
    /// child, even when `tools` is empty (inherit-all).
    pub disallowed_tools: Vec<String>,
    /// Optional model override: `"inherit"`/empty inherits the parent model,
    /// anything else names a model in the host registry (bare id or
    /// `provider_id/model_id`). An unresolvable value fails the spawn instead
    /// of silently falling back, so a profile never runs on the wrong model.
    pub model: Option<String>,
    /// Read-only profiles are highlighted in the UI and cannot mutate files.
    pub read_only: bool,
    /// Whether this profile may launch its own children. The host still caps
    /// the absolute depth, so a malicious prompt cannot recurse forever.
    pub allow_spawn: bool,
    pub color: SubagentColor,
    pub source: ProfileSource,
}

/// Where a profile came from. The UI groups the catalog by source and the
/// loader uses the ordering to decide overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProfileSource {
    #[default]
    BuiltIn,
    /// `~/.effibuddy/agents/*.md` (or the OS app-data equivalent).
    User,
    /// `<workspace>/.effibuddy/agents/*.md`; overrides user files by name.
    Project,
}

impl ProfileSource {
    pub fn label(self) -> &'static str {
        match self {
            ProfileSource::BuiltIn => "内置",
            ProfileSource::User => "用户",
            ProfileSource::Project => "项目",
        }
    }
}

impl SubagentProfile {
    /// Whether this profile may use `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        if tool == "agent" {
            return self.allow_spawn;
        }
        if self
            .disallowed_tools
            .iter()
            .any(|name| name == tool || name == "*")
        {
            return false;
        }
        self.tools.is_empty() || self.tools.iter().any(|name| name == tool)
    }

    /// Whether the model field asks for the launcher's model.
    pub fn inherits_model(&self) -> bool {
        self.model
            .as_deref()
            .map(|value| {
                let value = value.trim();
                value.is_empty() || value.eq_ignore_ascii_case("inherit")
            })
            .unwrap_or(true)
    }
}

/// Built-in agent types. These are the profiles announced in the `agent`
/// tool description and accepted by `subagent_type`.
pub fn builtin_subagent_profiles() -> Vec<SubagentProfile> {
    vec![
        SubagentProfile {
            name: "general-purpose".to_string(),
            description: "通用子代理：可读写文件、运行命令，独立完成一段多步任务。".to_string(),
            prompt: r#"You are a general-purpose subagent launched by EffiBuddy.

Complete the task your launcher gave you. You have your own tool set and your
own transcript; the launcher only receives your final report.

Rules:
- Investigate before acting. Read a file before editing it.
- Work inside the workspace root. Never try to escape it.
- You may launch child agents with the `agent` tool when it is present in your
  tool list. Use them for genuinely independent work, keep the tree shallow,
  and never create a chain just to pass messages along.
- Do not ask the user questions; nobody can answer you mid-run. If a choice is
  genuinely blocking, explain the options in your report instead of guessing.
- Do not create report, summary, or findings Markdown files. Return findings
  directly in your final message.
- Refer to files with paths relative to the workspace root.
- When done, write a concise report: what you found or changed, what you
  verified, and anything the launcher must know next. Do not use emojis."#
                .to_string(),
            tools: Vec::new(),
            read_only: false,
            allow_spawn: true,
            color: SubagentColor::Blue,
            disallowed_tools: Vec::new(),
            model: None,
            source: ProfileSource::BuiltIn,
        },
        SubagentProfile {
            name: "explore".to_string(),
            description: "只读探索：快速搜索代码、确认事实与调用关系，不改动任何文件。".to_string(),
            prompt: r#"You are EffiBuddy Explore, a read-only codebase research specialist.

Your job is to locate facts quickly and report them with precise references.

Hard rules:
- READ-ONLY MODE. Do not create, modify, delete, move, or rename files. Do not
  write temporary files. Do not run shell commands that change state.
- You may launch read-only child agents with the `agent` tool when it is
  available. Children inherit read-only mode.
- Prefer `grep` and `list_dir` to find candidates, then `read_file` for the
  relevant regions. Read before you claim.
- Do not open with a plan; start searching immediately.
- Finish with a compact report: the answer, the evidence (path and line
  numbers), and any uncertainty. No emojis, no report files."#
                .to_string(),
            tools: vec![
                "read_file".to_string(),
                "list_dir".to_string(),
                "grep".to_string(),
            ],
            read_only: true,
            allow_spawn: true,
            color: SubagentColor::Green,
            disallowed_tools: Vec::new(),
            model: None,
            source: ProfileSource::BuiltIn,
        },
        SubagentProfile {
            name: "reviewer".to_string(),
            description: "只读审查：从正确性、安全与测试覆盖角度检查改动，给出可执行结论。"
                .to_string(),
            prompt: r#"You are EffiBuddy Reviewer, a read-only code review specialist.

Review the change or area described by your launcher. Find real problems, not
style preferences.

Focus, in order:
1. Correctness bugs and broken invariants.
2. Security and data-loss risks (path escapes, destructive commands, secrets).
3. Missing tests or verification for the changed behaviour.
4. Concurrency, cancellation and error-handling gaps.

Hard rules:
- READ-ONLY MODE. Do not modify files or run mutating commands.
- You may launch read-only child agents with the `agent` tool when it is
  available. Children inherit read-only mode.
- Every finding needs a file path, a line reference if possible, and a concrete
  failure scenario. Distinguish "definite bug" from "worth checking".
- If you find nothing material, say so plainly and list what you verified.
- Return the review as your final message. No emojis, no report files."#
                .to_string(),
            tools: vec![
                "read_file".to_string(),
                "list_dir".to_string(),
                "grep".to_string(),
            ],
            read_only: true,
            allow_spawn: true,
            color: SubagentColor::Purple,
            disallowed_tools: Vec::new(),
            model: None,
            source: ProfileSource::BuiltIn,
        },
    ]
}

pub fn find_profile(name: &str) -> Option<SubagentProfile> {
    let normalized = normalize_profile_name(name);
    builtin_subagent_profiles()
        .into_iter()
        .find(|profile| normalize_profile_name(&profile.name) == normalized)
}

/// Tolerant matching: `General Purpose`, `general_purpose` and
/// `general-purpose` all resolve to the same profile.
pub fn normalize_profile_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

/// What the parent tool asks for. `parent_call_id` is the UI anchor; it is
/// never overwritten when an agent is resumed.
#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub profile: String,
    pub description: String,
    pub prompt: String,
    pub background: bool,
    pub parent_session_id: String,
    pub parent_call_id: String,
    pub workspace: PathBuf,
    /// Model-facing handle. Assigned by the host when absent; duplicates get a
    /// short numeric suffix instead of failing the spawn.
    pub agent_name: Option<String>,
    /// Identity of the launching agent (`main` for the top-level harness).
    pub parent_agent_id: Option<String>,
    pub parent_agent_name: Option<String>,
    /// 1 for a direct child of the main agent, 2 for a grandchild, and so on.
    pub depth: usize,
    /// Replaces the profile's system prompt body when present.
    pub system_prompt: Option<String>,
    /// Restricts the child tool surface. `None` inherits the profile.
    pub tools: Option<Vec<String>>,
    /// Whether this child may launch its own children.
    pub allow_spawn: bool,
    /// Overrides the profile's read-only flag when present.
    pub read_only: Option<bool>,
    /// When set, the child's final output must be JSON matching this schema.
    pub output_schema: Option<Value>,
    /// Who may address this agent object later. Private objects belong to the
    /// creating agent; public objects can be reused by siblings.
    pub visibility: AgentVisibility,
    /// Agent-group metadata (workflow runs).
    pub group_id: Option<String>,
    pub group_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    /// The session was restored after a restart and this child has no
    /// recorded terminal state: the process exited before it finished.
    /// Never produced by a live run; the host derives it from disk.
    Lost,
}

impl SubagentStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, SubagentStatus::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            SubagentStatus::Running => "运行中",
            SubagentStatus::Completed => "已完成",
            SubagentStatus::Failed => "失败",
            SubagentStatus::Cancelled => "已停止",
            SubagentStatus::Lost => "已丢失",
        }
    }
}

/// Returned immediately for a background launch.
#[derive(Clone, Debug)]
pub struct SubagentLaunch {
    pub agent_id: String,
    pub agent_name: String,
    pub child_session_id: String,
}

/// Terminal (or partial) result of one subagent run.
#[derive(Clone, Debug)]
pub struct SubagentReport {
    pub agent_id: String,
    pub name: String,
    pub profile: String,
    pub color: SubagentColor,
    pub description: String,
    pub child_session_id: String,
    pub parent_call_id: String,
    pub parent_agent_id: Option<String>,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    /// Visibility of the agent object, echoed for the UI and downstream tools.
    pub visibility: AgentVisibility,
    pub background: bool,
    /// True when this run was started by an incoming agent message rather than
    /// by the launcher. The UI can avoid notifying the parent twice.
    pub wake: bool,
    pub status: SubagentStatus,
    /// Final assistant text. On failure this may be the latest partial output.
    pub report: String,
    /// Parsed and schema-validated JSON output, when the request declared a
    /// contract. Also populated for workflow nodes without a schema when the
    /// final text happens to contain JSON.
    pub data: Option<Value>,
    pub error: Option<String>,
    pub tool_uses: usize,
    pub duration_ms: u64,
    pub usage: Usage,
}

/// The runtime boundary between the model-visible `agent` tool and the host.
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    fn profiles(&self) -> Vec<SubagentProfile>;

    /// Run to completion on the caller's task. Implementations should ensure
    /// the child keeps cleaning up if this future is dropped.
    async fn run(&self, request: SubagentRequest, cancel: CancellationToken) -> SubagentReport;

    /// Detach the run and return the identifiers immediately.
    fn launch(&self, request: SubagentRequest) -> Result<SubagentLaunch, ToolError>;

    /// Ask a running subagent to stop. Unknown or terminal ids are ignored.
    fn stop(&self, agent_id: &str);

    /// Addressable agents known to this host, including the main agent and
    /// group nodes. Used by the relationship graph.
    fn agents(&self) -> Vec<AgentDescriptor> {
        Vec::new()
    }

    /// Agents the given viewer may address. Private objects are visible only to
    /// their owner; public objects and the main agent are visible to everyone.
    fn visible_agents(&self, _viewer_agent_id: &str) -> Vec<AgentDescriptor> {
        self.agents()
    }

    /// Deliver a message from `from_session` to an agent address (id or name).
    /// Implementations decide whether the target is woken immediately.
    fn send_agent_message(
        &self,
        _from_session: &str,
        _to: &str,
        _kind: AgentMessageKind,
        _text: String,
        _data: Option<Value>,
    ) -> Result<AgentSendOutcome, ToolError> {
        Err(ToolError::new(
            "Agent messaging is not available in this host.",
        ))
    }

    /// Route an approval decision to the child that requested it.
    fn respond_approval(
        &self,
        _agent_id: &str,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> bool {
        false
    }

    fn stop_all(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_profile_can_opt_into_children() {
        let profiles = builtin_subagent_profiles();
        assert!(
            profiles
                .iter()
                .any(|profile| profile.name == "general-purpose")
        );
        assert!(profiles.iter().any(|profile| profile.name == "explore"));
        assert!(profiles.iter().any(|profile| profile.name == "reviewer"));
        for profile in profiles {
            assert!(profile.allow_spawn, "{}", profile.name);
            assert!(profile.allows("agent"), "{}", profile.name);
        }
    }

    #[test]
    fn profile_names_match_tolerantly() {
        assert_eq!(find_profile("Explore").unwrap().name, "explore");
        assert_eq!(
            find_profile("General Purpose").unwrap().name,
            "general-purpose"
        );
        assert!(find_profile("nope").is_none());
    }

    #[test]
    fn read_only_profiles_do_not_allow_mutating_tools() {
        let explore = find_profile("explore").unwrap();
        assert!(explore.read_only);
        assert!(explore.allows("read_file"));
        assert!(!explore.allows("write_file"));
        assert!(!explore.allows("shell"));
    }
}
