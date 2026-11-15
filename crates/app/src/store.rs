//! Session persistence: one JSON file per conversation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use effibuddy_agent_core::SessionState;

use crate::config::base_dir;

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    /// Project the conversation belongs to; `None` puts it in the sidebar's
    /// 任务 section. Legacy transcripts only carry this path; `project_id`
    /// (when present) is authoritative.
    pub project: Option<PathBuf>,
    pub project_id: Option<String>,
    pub pinned: bool,
    pub unread: bool,
}

/// One cached sidebar summary, keyed by file `(mtime, len)`.
#[derive(Clone)]
struct CachedSummary {
    mtime: SystemTime,
    len: u64,
    /// `None` when the file failed to parse.
    summary: Option<SessionSummary>,
    parent_id: Option<String>,
    archived: bool,
}

#[derive(Clone)]
pub struct SessionStore {
    directory: PathBuf,
    /// Session ids deleted while saves may still be queued. A queued write
    /// checks this set under the same lock the delete uses, so a transcript
    /// removed from the archive page cannot be resurrected by a stale save.
    deleted: Arc<Mutex<HashSet<String>>>,
    /// Sidebar summaries keyed by file path. A full refresh used to parse
    /// every JSON file on every scan — O(total bytes), which pins the UI
    /// thread with hundreds of sessions. Metadata-guarded caching limits
    /// parsing to files whose mtime or size changed.
    summary_cache: Arc<Mutex<HashMap<PathBuf, CachedSummary>>>,
}

/// A serialized session produced by [`SessionStore::stage`], waiting to be
/// written. Writes happen off the UI thread; sends into the writer queue are
/// ordered, so the last staged save of a session wins on disk.
pub struct PendingSave {
    directory: PathBuf,
    id: String,
    json: String,
    deleted: Arc<Mutex<HashSet<String>>>,
}

impl PendingSave {
    /// Write the staged JSON to disk (atomic rename, same as [`SessionStore::save`]).
    ///
    /// Takes `&self` so the caller can write synchronously and then enqueue the
    /// same serialized payload behind older staged writes without serializing
    /// the session a second time.
    pub fn write(&self) -> Result<(), String> {
        // Hold the lock across the write so a concurrent delete either waits
        // for this write and removes the file afterwards, or lands first and
        // makes this write a no-op.
        let deleted = self
            .deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if deleted.contains(&self.id) {
            return Ok(());
        }
        let path = self.directory.join(format!("{}.json", self.id));
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &self.json)
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("cannot finalize {}: {error}", path.display()))
    }
}

impl SessionStore {
    pub fn new() -> Self {
        let preferred = base_dir().join("sessions");
        let directory = if std::fs::create_dir_all(&preferred).is_ok() {
            preferred
        } else {
            let fallback = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".effibuddy")
                .join("sessions");
            let _ = std::fs::create_dir_all(&fallback);
            fallback
        };
        Self {
            directory,
            deleted: Arc::default(),
            summary_cache: Arc::default(),
        }
    }

    /// Test/preview store rooted at an explicit directory.
    #[allow(dead_code)]
    pub fn with_directory(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        let _ = std::fs::create_dir_all(&directory);
        Self {
            directory,
            deleted: Arc::default(),
            summary_cache: Arc::default(),
        }
    }

    pub fn directory(&self) -> &PathBuf {
        &self.directory
    }

    pub fn load(&self, id: &str) -> Option<SessionState> {
        let path = self.path_for(id);
        let contents = std::fs::read_to_string(&path).ok()?;
        let mut session = session_from_json(&contents)?;
        session.normalize_after_load();
        Some(session)
    }

    /// Load without reopening side effects (`normalize_after_load` refreshes
    /// `updated_at` and repairs tool calls). Used for metadata-only edits such
    /// as pin, archive and restore so reordering the sidebar is left intact.
    pub fn load_for_update(&self, id: &str) -> Option<SessionState> {
        let contents = std::fs::read_to_string(self.path_for(id)).ok()?;
        session_from_json(&contents)
    }

    /// Raw transcript read for export: no status/`updated_at` rewrite and no
    /// dangling-tool-call repair. The archive mirrors what was saved.
    pub fn load_raw(&self, id: &str) -> Option<SessionState> {
        self.load_for_update(id)
    }

    pub fn save(&self, session: &SessionState) -> Result<(), String> {
        let path = self.path_for(&session.id);
        let json = serde_json::to_string_pretty(session)
            .map_err(|error| format!("cannot serialize session: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("cannot finalize {}: {error}", path.display()))
    }

    /// Serialize a session without touching the filesystem, so the write can
    /// happen off the UI thread ([`PendingSave::write`]).
    pub fn stage(&self, session: &SessionState) -> Result<PendingSave, String> {
        let json = serde_json::to_string_pretty(session)
            .map_err(|error| format!("cannot serialize session: {error}"))?;
        Ok(PendingSave {
            directory: self.directory.clone(),
            id: session.id.clone(),
            json,
            deleted: self.deleted.clone(),
        })
    }

    /// Subagent sessions whose `parent_id` matches. Used to restore the
    /// agent catalog of a reopened parent session.
    pub fn child_sessions(&self, parent_id: &str) -> Vec<SessionState> {
        self.collect_child_sessions(parent_id, true)
    }

    /// Child transcripts exactly as saved, without the reopen-time
    /// normalization that rewrites `updated_at` and repairs tool calls.
    /// Session export uses this so the archive mirrors the transcript.
    pub fn child_sessions_raw(&self, parent_id: &str) -> Vec<SessionState> {
        self.collect_child_sessions(parent_id, false)
    }

    fn collect_child_sessions(&self, parent_id: &str, normalize: bool) -> Vec<SessionState> {
        let mut children = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return children;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(mut session) = session_from_json(&contents) else {
                continue;
            };
            if session.parent_id.as_deref() != Some(parent_id) {
                continue;
            }
            if normalize {
                session.normalize_after_load();
            }
            children.push(session);
        }
        children.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        children
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        // Register the tombstone under the same lock a queued write holds
        // across its rename, so either the write finishes first and this
        // delete removes the file afterwards, or the write sees the tombstone
        // and skips.
        self.deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string());
        let path = self.path_for(id);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cannot delete {}: {error}", path.display()))?;
        }
        Ok(())
    }

    pub fn summaries(&self) -> Vec<SessionSummary> {
        self.collect_summaries(false)
    }

    /// Conversations archived from the sidebar, shown on the settings page.
    pub fn archived_summaries(&self) -> Vec<SessionSummary> {
        self.collect_summaries(true)
    }

    fn collect_summaries(&self, archived: bool) -> Vec<SessionSummary> {
        let mut summaries = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return summaries;
        };
        let mut cache = self
            .summary_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut seen: HashMap<PathBuf, ()> = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let len = meta.len();
            seen.insert(path.clone(), ());

            let cached = match cache.get(&path) {
                Some(cached) if cached.mtime == mtime && cached.len == len => cached.clone(),
                _ => {
                    // Parse only files whose metadata changed since the last
                    // scan.
                    let parsed = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|contents| session_from_json(&contents))
                        .map(|session| {
                            let summary = SessionSummary {
                                id: session.id.clone(),
                                title: session.display_title(),
                                updated_at: session.updated_at,
                                message_count: session.messages.len(),
                                project: session.project.clone(),
                                project_id: session.project_id.clone(),
                                pinned: session.pinned,
                                unread: session.unread,
                            };
                            CachedSummary {
                                mtime,
                                len,
                                summary: Some(summary),
                                parent_id: session.parent_id,
                                archived: session.archived,
                            }
                        })
                        .unwrap_or_else(|| CachedSummary {
                            mtime,
                            len,
                            summary: None,
                            parent_id: None,
                            archived: false,
                        });
                    cache.insert(path.clone(), parsed.clone());
                    parsed
                }
            };

            // Subagent transcripts live in the same directory but are opened
            // from the agent panel, never listed as top-level sessions.
            if cached.parent_id.is_some() || cached.archived != archived {
                continue;
            }
            if let Some(summary) = cached.summary {
                // A conversation that has not sent its first user message is a
                // composer draft (see `draft_store`), not history. Keeping it
                // out of every sidebar/archive scan is what makes repeated
                // “新建任务” clicks reuse one invisible session.
                if summary.message_count == 0 {
                    continue;
                }
                summaries.push(summary);
            }
        }
        // Drop cache entries for deleted files so the map cannot grow
        // without bound across archives/restores.
        cache.retain(|path, _| seen.contains_key(path));
        summaries.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        summaries
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.json"))
    }
}

/// Parse a transcript and migrate pre-project files: a top-level session
/// written before the `project` field existed belonged to its `cwd` project.
/// New files always carry `project` (possibly `null`), so `None` is preserved
/// for tasks.
fn session_from_json(contents: &str) -> Option<SessionState> {
    let value: serde_json::Value = serde_json::from_str(contents).ok()?;
    let needs_project_migration = value.get("project").is_none();
    let mut session: SessionState = serde_json::from_value(value).ok()?;
    if needs_project_migration && session.parent_id.is_none() {
        session.project = Some(session.cwd.clone());
    }
    Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: &str, cwd: &str) -> SessionState {
        let mut session = SessionState::new(PathBuf::from(cwd), "test-model");
        session.push_message(effibuddy_agent_core::Message::user(title.to_string()));
        session.title = title.to_string();
        session
    }

    #[test]
    fn unsent_drafts_stay_out_of_history() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        store
            .save(&SessionState::new(PathBuf::from("/tmp/workspace"), "test-model"))
            .unwrap();

        assert!(store.summaries().is_empty());
        assert!(store.archived_summaries().is_empty());
    }

    #[test]
    fn legacy_transcripts_inherit_the_cwd_project() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let mut legacy = session("legacy", "/tmp/legacy-project");
        store.save(&legacy).unwrap();

        // Simulate a file written before the `project` field existed.
        let path = directory.path().join(format!("{}.json", legacy.id));
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("project");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].project.as_deref(),
            Some(std::path::Path::new("/tmp/legacy-project"))
        );

        legacy.title = "changed".to_string();
        let loaded = store.load(&legacy.id).unwrap();
        assert_eq!(loaded.project, Some(PathBuf::from("/tmp/legacy-project")));
    }

    #[test]
    fn new_task_projects_stay_unassigned() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let task = session("task", "/tmp/workspace");
        assert!(task.project.is_none());
        store.save(&task).unwrap();

        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].project.is_none());
    }

    #[test]
    fn archived_sessions_are_listed_separately() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let mut archived = session("done", "/tmp/project");
        archived.archived = true;
        let open = session("open", "/tmp/project");
        store.save(&archived).unwrap();
        store.save(&open).unwrap();

        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "open");

        let archived_summaries = store.archived_summaries();
        assert_eq!(archived_summaries.len(), 1);
        assert_eq!(archived_summaries[0].title, "done");
    }

    #[test]
    fn raw_load_preserves_the_saved_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let mut session = session("export", "/tmp/project");
        let stamp = "2026-11-01T00:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        session.updated_at = stamp;
        store.save(&session).unwrap();

        let raw = store.load_raw(&session.id).unwrap();
        assert_eq!(raw.updated_at, stamp);
    }

    #[test]
    fn summaries_carry_the_unread_marker() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let mut unread = session("unread", "/tmp/project");
        unread.unread = true;
        store.save(&unread).unwrap();

        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].unread);
    }

    #[test]
    fn queued_save_cannot_resurrect_a_deleted_session() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let task = session("task", "/tmp/workspace");
        store.save(&task).unwrap();

        // A save staged before the delete must be a no-op afterwards.
        let stale = store.stage(&task).unwrap();
        store.delete(&task.id).unwrap();
        stale.write().unwrap();

        assert!(store.load_for_update(&task.id).is_none());
        assert!(store.summaries().is_empty());
    }
}
