//! Composer drafts and unsent new-task sessions.
//!
//! Two kinds of UI state survive a session switch or an app restart but are
//! not part of the model-visible transcript:
//!
//! * the text typed into the composer for a given conversation, and
//! * the one not-yet-sent conversation each project (or the 任务 section)
//!   owns, so pressing “新建任务” repeatedly returns to the same draft instead
//!   of spawning empty transcripts and losing the typed prompt.
//!
//! Both live in a small `drafts.json` next to the session transcripts. Keeping
//! them out of `SessionState` means a keystroke never forces a megabyte-scale
//! transcript serialization, and unsent sessions can stay hidden from the
//! sidebar until their first user message is actually sent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use effibuddy_agent_core::UserAttachment;
use serde::{Deserialize, Serialize};

use crate::config::base_dir;

/// Scope key for the sidebar's 任务 section.
pub const TASK_SCOPE: &str = "task";

/// Stable cache key for a project group. Registered projects use their
/// registry id; legacy groups fall back to the normalized primary folder.
pub fn scope_key(project_id: Option<&str>, project: Option<&Path>) -> String {
    if let Some(id) = project_id.filter(|id| !id.is_empty()) {
        return format!("project:{id}");
    }
    match project {
        Some(path) => format!(
            "legacy:{}",
            crate::project_store::normalize_root(path).display()
        ),
        None => TASK_SCOPE.to_string(),
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftState {
    pub text: String,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
}

impl Default for DraftState {
    fn default() -> Self {
        Self {
            text: String::new(),
            updated_at: now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DraftFile {
    #[serde(default = "default_version")]
    version: u32,
    /// Session id → composer text.
    #[serde(default)]
    drafts: BTreeMap<String, DraftState>,
    /// Project/task scope → the id of its single unsent conversation.
    #[serde(default)]
    new_tasks: BTreeMap<String, String>,
    /// Conversation opened when the app was last closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_active: Option<String>,
}

fn default_version() -> u32 {
    1
}

impl Default for DraftFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            drafts: BTreeMap::new(),
            new_tasks: BTreeMap::new(),
            last_active: None,
        }
    }
}

/// In-memory draft state backed by one JSON file.
pub struct DraftStore {
    path: PathBuf,
    file: DraftFile,
}

impl DraftStore {
    pub fn new() -> Self {
        Self::with_path(base_dir().join("drafts.json"))
    }

    /// Test/preview store rooted at an explicit file.
    #[allow(dead_code)]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        Self { path, file }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn text(&self, session_id: &str) -> &str {
        self.file
            .drafts
            .get(session_id)
            .map(|draft| draft.text.as_str())
            .unwrap_or_default()
    }

    /// Store the composer text for one conversation. Empty text drops the
    /// record; the return value says whether anything changed, so callers can
    /// schedule a debounced save only when needed.
    pub fn set_text(&mut self, session_id: &str, text: String) -> bool {
        if text.is_empty() {
            return self.file.drafts.remove(session_id).is_some();
        }
        match self.file.drafts.get_mut(session_id) {
            Some(existing) if existing.text == text => false,
            Some(existing) => {
                existing.text = text;
                existing.updated_at = now();
                true
            }
            None => {
                self.file.drafts.insert(
                    session_id.to_string(),
                    DraftState {
                        text,
                        updated_at: now(),
                    },
                );
                true
            }
        }
    }

    /// The unsent conversation registered for a project/task scope.
    pub fn scope_session(&self, scope: &str) -> Option<&str> {
        self.file.new_tasks.get(scope).map(String::as_str)
    }

    /// Whether this conversation is still one of the scope-owned unsent
    /// drafts. Used to validate `last_active` on launch and on session switch.
    pub fn is_unsent(&self, session_id: &str) -> bool {
        self.file
            .new_tasks
            .values()
            .any(|candidate| candidate == session_id)
    }

    /// Remember `session_id` as the scope's unsent conversation. A scope owns
    /// exactly one, so any previous binding is replaced.
    pub fn bind_scope(&mut self, scope: &str, session_id: &str) {
        self.file
            .new_tasks
            .insert(scope.to_string(), session_id.to_string());
    }

    pub fn last_active(&self) -> Option<&str> {
        self.file.last_active.as_deref()
    }

    pub fn set_last_active(&mut self, session_id: &str) -> bool {
        if self.file.last_active.as_deref() == Some(session_id) {
            return false;
        }
        self.file.last_active = Some(session_id.to_string());
        true
    }

    /// Forget everything about a conversation: its composer draft and every
    /// scope that pointed at it. Returns true when a record was removed, so
    /// callers can detect "the draft just became a real conversation" even
    /// after the composer has already been cleared by submitting.
    pub fn remove_session(&mut self, session_id: &str) -> bool {
        let removed_text = self.file.drafts.remove(session_id).is_some();
        let removed_scope = self.unbind_session(session_id);
        self.clear_attachments(session_id);
        removed_text || removed_scope
    }

    /// Drop every scope binding pointing at `session_id` while keeping the
    /// conversation's composer text. Used when the first user message turns a
    /// draft into history: any text typed after submitting belongs to the next
    /// turn and must survive.
    pub fn unbind_session(&mut self, session_id: &str) -> bool {
        let before = self.file.new_tasks.len();
        self.file
            .new_tasks
            .retain(|_, candidate| candidate != session_id);
        before != self.file.new_tasks.len()
    }

    fn attachment_directory(&self) -> PathBuf {
        match self.path.parent() {
            Some(parent) => parent.join("draft_attachments"),
            None => PathBuf::from("draft_attachments"),
        }
    }

    fn attachment_path(&self, session_id: &str) -> PathBuf {
        self.attachment_directory()
            .join(format!("{session_id}.json"))
    }

    /// Files/images/skills staged for the next message. Stored in a separate
    /// per-session file: an image payload can reach several megabytes and must
    /// not be rewritten every time the text draft changes.
    pub fn attachments(&self, session_id: &str) -> Vec<UserAttachment> {
        let Ok(contents) = std::fs::read_to_string(self.attachment_path(session_id)) else {
            return Vec::new();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }

    pub fn set_attachments(
        &self,
        session_id: &str,
        attachments: &[UserAttachment],
    ) -> Result<(), String> {
        if attachments.is_empty() {
            self.clear_attachments(session_id);
            return Ok(());
        }
        let directory = self.attachment_directory();
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        let path = self.attachment_path(session_id);
        let json = serde_json::to_string(attachments)
            .map_err(|error| format!("cannot serialize attachments: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("cannot finalize {}: {error}", path.display()))
    }

    pub fn clear_attachments(&self, session_id: &str) {
        let path = self.attachment_path(session_id);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(parent) = self.path.parent() else {
            return Err("drafts.json 路径无效".to_string());
        };
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        let json = serde_json::to_string_pretty(&self.file)
            .map_err(|error| format!("cannot serialize drafts: {error}"))?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| format!("cannot finalize {}: {error}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_round_trip_through_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.json");
        let mut store = DraftStore::with_path(&path);
        assert!(store.set_text("session-a", "写到一半的提示词".to_string()));
        assert!(!store.set_text("session-a", "写到一半的提示词".to_string()));
        store.bind_scope("project:p1", "session-a");
        store.set_last_active("session-a");
        store.save().unwrap();

        let reloaded = DraftStore::with_path(&path);
        assert_eq!(reloaded.text("session-a"), "写到一半的提示词");
        assert_eq!(reloaded.scope_session("project:p1"), Some("session-a"));
        assert_eq!(reloaded.last_active(), Some("session-a"));
    }

    #[test]
    fn empty_text_drops_the_record() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = DraftStore::with_path(directory.path().join("drafts.json"));
        store.set_text("session-a", "hello".to_string());
        assert!(store.set_text("session-a", String::new()));
        assert_eq!(store.text("session-a"), "");
    }

    #[test]
    fn removing_a_session_clears_its_scope_binding() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = DraftStore::with_path(directory.path().join("drafts.json"));
        store.set_text("session-a", "hello".to_string());
        store.bind_scope("project:p1", "session-a");
        store.bind_scope("task", "session-b");
        assert!(store.remove_session("session-a"));
        assert!(store.scope_session("project:p1").is_none());
        assert_eq!(store.scope_session("task"), Some("session-b"));
        // A second removal has nothing left to do.
        assert!(!store.remove_session("session-a"));
    }

    #[test]
    fn attachments_round_trip_and_are_cleared_with_the_session() {
        use effibuddy_agent_core::{AttachmentKind, UserAttachment};

        let directory = tempfile::tempdir().unwrap();
        let mut store = DraftStore::with_path(directory.path().join("drafts.json"));
        let attachment = UserAttachment {
            name: "notes.md".to_string(),
            path: "notes.md".to_string(),
            kind: AttachmentKind::File,
            mime_type: "text/markdown".to_string(),
            size: 4,
            text: Some("# hi".to_string()),
            image: None,
            truncated: false,
        };
        store
            .set_attachments("session-a", std::slice::from_ref(&attachment))
            .unwrap();
        let restored = store.attachments("session-a");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, "notes.md");
        assert_eq!(restored[0].text.as_deref(), Some("# hi"));

        store.remove_session("session-a");
        assert!(store.attachments("session-a").is_empty());
    }

    #[test]
    fn unbinding_keeps_the_composer_text_for_the_next_turn() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = DraftStore::with_path(directory.path().join("drafts.json"));
        store.set_text("session-a", "next prompt".to_string());
        store.bind_scope("task", "session-a");
        assert!(store.unbind_session("session-a"));
        assert!(store.scope_session("task").is_none());
        assert_eq!(store.text("session-a"), "next prompt");
        assert!(!store.unbind_session("session-a"));
    }

    #[test]
    fn scopes_separate_tasks_from_projects() {
        assert_eq!(scope_key(None, None), TASK_SCOPE);
        assert_eq!(scope_key(Some("p1"), None), "project:p1");
        let path = std::env::temp_dir().join("effibuddy-scope-test");
        let scope = scope_key(None, Some(&path));
        assert!(scope.starts_with("legacy:"));
        // The same project always produces the same key.
        assert_eq!(scope, scope_key(None, Some(&path)));
    }
}
