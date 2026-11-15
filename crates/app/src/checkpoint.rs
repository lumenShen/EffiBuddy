//! VCS-free file checkpoints behind message edit and undo.
//!
//! Every file mutation reported by a tool is recorded against the user message
//! that owns the turn. The checkpoint stores the original bytes plus the digest
//! of the tool's last write, so a rewind can (a) tell whether another process
//! touched the file after the agent and (b) restore the exact pre-turn state.
//!
//! Deliberately no Git is involved: snapshots live under the app data
//! directory and a restore is a plain atomic filesystem write. This also means
//! a rewind cannot be recovered from a repository — the confirmation UI says so
//! explicitly.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use effibuddy_agent_core::{
    ContentDigest, FileCheckpoint, FileCheckpointSink, SessionState, SharedSession, lock_session,
};

/// Keep enough history for normal edit/undo use without letting snapshot
/// storage grow without bound. Older turns fall back to conversation-only
/// rewinds (the UI reports that no reversible file changes remain).
const MAX_TURNS: usize = 64;

/// Do not copy files larger than this into the checkpoint store. A tool can
/// still touch such a file, but the rewind layer refuses to hold a second copy
/// and reports a missing snapshot instead of risking an unbounded allocation.
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn checkpoint_root() -> PathBuf {
    crate::config::base_dir().join("checkpoints")
}

fn session_dir(root: &Path, session_id: &str) -> PathBuf {
    root.join(session_id)
}

fn turn_dir(root: &Path, session_id: &str, message_id: &str) -> PathBuf {
    session_dir(root, session_id).join(message_id)
}

/// Maximum number of file rows rendered in a confirmation dialog. The full
/// count is still reported; this only bounds the element tree.
pub(crate) const MAX_LISTED_FILES: usize = 120;

/// Snapshot names must be unique before the session lock is taken: the blob is
/// written outside the lock so a multi-megabyte snapshot cannot stall chat
/// rebuilds or concurrent subagent writes. A rare losing race deletes the
/// orphan blob after the metadata re-check.
static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_snapshot_name() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{stamp:032x}-{seq:08x}.bin")
}

/// Records the pre-mutation state of files edited during one session run.
///
/// A recorder is installed in [`effibuddy_agent_core::HarnessConfig`] for every
/// parent run. Child harnesses inherit the same `Arc`, so subagent edits land in
/// the parent user turn's checkpoint.
pub(crate) struct CheckpointRecorder {
    session: SharedSession,
    session_id: String,
    root: PathBuf,
    turn: Mutex<Option<String>>,
}

impl CheckpointRecorder {
    pub(crate) fn new(session: SharedSession) -> Self {
        let session_id = lock_session(&session).id.clone();
        Self {
            session,
            session_id,
            root: checkpoint_root(),
            turn: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_root(session: SharedSession, root: PathBuf) -> Self {
        let session_id = lock_session(&session).id.clone();
        Self {
            session,
            session_id,
            root,
            turn: Mutex::new(None),
        }
    }

    /// Bind this recorder to a specific user turn. Called on `RunStarted`,
    /// before any tool can run, so delayed background subagents keep writing
    /// to the turn that spawned them even if a new user message arrives later.
    pub(crate) fn bind_turn(&self, message_id: impl Into<String>) {
        *self
            .turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message_id.into());
    }

    /// The visible user message this run belongs to. Background notifications
    /// and synthetic messages are skipped, so a wake-up run keeps appending to
    /// the turn the user typed.
    fn turn_id(&self) -> Option<String> {
        if let Some(id) = self
            .turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return Some(id.clone());
        }
        let turn_id = {
            let session = lock_session(&self.session);
            session.latest_user_turn_id()?.to_string()
        };
        *self
            .turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(turn_id.clone());
        Some(turn_id)
    }
}

impl FileCheckpointSink for CheckpointRecorder {
    fn before_write(&self, relative_path: &str, absolute_path: &Path, tool: &str) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };

        // Fast path: the first write of the turn already captured the file.
        {
            let session = lock_session(&self.session);
            if session
                .checkpoint(&turn_id)
                .is_some_and(|checkpoint| checkpoint.files.iter().any(|f| f.path == relative_path))
            {
                return;
            }
        }

        // Snapshot outside the session lock; a large file must not stall the
        // UI's transcript rebuild.
        let metadata = fs::symlink_metadata(absolute_path);
        let existed_before = metadata.as_ref().is_ok_and(|meta| meta.is_file());
        let unsupported = metadata.as_ref().is_ok_and(|meta| meta.is_dir());
        let metadata_failed = metadata
            .as_ref()
            .is_err_and(|error| error.kind() != io::ErrorKind::NotFound);
        let oversize = metadata
            .as_ref()
            .is_ok_and(|meta| meta.is_file() && meta.len() > MAX_SNAPSHOT_BYTES);
        let snapshot_bytes = if existed_before && !oversize {
            fs::read(absolute_path)
                .ok()
                .filter(|bytes| bytes.len() as u64 <= MAX_SNAPSHOT_BYTES)
        } else {
            None
        };

        // Write the snapshot blob outside the session lock. A multi-megabyte
        // copy must not stall the UI's transcript rebuild or a concurrent
        // subagent's checkpoint insert; the metadata re-check below deletes
        // this blob if another agent won the race for the same path.
        let (snapshot, blob_failed) = match snapshot_bytes.as_ref() {
            Some(bytes) => {
                let name = new_snapshot_name();
                let dir = turn_dir(&self.root, &self.session_id, &turn_id);
                let written = fs::create_dir_all(&dir).is_ok()
                    && atomic_write(&dir.join(&name), bytes).is_ok();
                if written {
                    (Some(name), false)
                } else {
                    (None, true)
                }
            }
            None => (None, false),
        };

        let mut removed_turns: Vec<String> = Vec::new();
        {
            let mut session = lock_session(&self.session);
            // Another agent may have recorded this file while we snapshotted.
            if session
                .checkpoint(&turn_id)
                .is_some_and(|checkpoint| checkpoint.files.iter().any(|f| f.path == relative_path))
            {
                if let Some(name) = &snapshot {
                    let _ = fs::remove_file(
                        turn_dir(&self.root, &self.session_id, &turn_id).join(name),
                    );
                }
                return;
            }

            let mut uncertain = unsupported || metadata_failed || blob_failed;
            if existed_before && snapshot_bytes.is_none() && !oversize {
                // The original bytes could not be read; never pretend this
                // file can be restored.
                uncertain = true;
            }

            let checkpoint = session.checkpoint_or_insert(&turn_id);
            checkpoint.files.push(FileCheckpoint {
                path: relative_path.to_string(),
                existed_before,
                snapshot,
                expected_after: None,
                operation_count: 0,
                tool_names: vec![tool.to_string()],
                uncertain,
            });
            session.touch();

            // Bound history. Never evict the turn currently being recorded.
            while session.checkpoints.len() > MAX_TURNS {
                let Some(index) = session
                    .checkpoints
                    .iter()
                    .position(|checkpoint| checkpoint.message_id != turn_id)
                else {
                    break;
                };
                if let Some(removed) = session.checkpoints.get(index) {
                    removed_turns.push(removed.message_id.clone());
                }
                session.checkpoints.remove(index);
            }
        }
        for removed in removed_turns {
            let _ = fs::remove_dir_all(turn_dir(&self.root, &self.session_id, &removed));
        }
    }

    fn after_write(&self, relative_path: &str, _absolute_path: &Path, tool: &str, content: &[u8]) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        let mut session = lock_session(&self.session);
        let checkpoint = session.checkpoint_or_insert(&turn_id);
        if let Some(file) = checkpoint.file_mut(relative_path) {
            file.expected_after = Some(ContentDigest::of(content));
            file.operation_count = file.operation_count.saturating_add(1);
            if !file.tool_names.iter().any(|name| name == tool) {
                file.tool_names.push(tool.to_string());
            }
        }
        session.touch();
    }

    fn write_failed(&self, relative_path: &str, _absolute_path: &Path, _tool: &str) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        let mut session = lock_session(&self.session);
        if let Some(checkpoint) = session.checkpoint_mut(&turn_id)
            && let Some(file) = checkpoint.file_mut(relative_path)
        {
            file.uncertain = true;
        }
        session.touch();
    }

    fn note_shell(&self, command: &str) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        let mut session = lock_session(&self.session);
        let checkpoint = session.checkpoint_or_insert(&turn_id);
        if !checkpoint
            .shell_commands
            .iter()
            .any(|existing| existing == command)
        {
            checkpoint.shell_commands.push(command.to_string());
        }
        session.touch();
    }
}

// --- restore planning ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreAction {
    /// Put the original bytes back.
    Restore,
    /// The file did not exist before the turn.
    Delete,
}

impl RestoreAction {
    pub fn label(self) -> &'static str {
        match self {
            RestoreAction::Restore => "还原",
            RestoreAction::Delete => "删除",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RestoreFile {
    pub path: String,
    pub action: RestoreAction,
    pub operations: u32,
    pub tool_names: Vec<String>,
    /// The content currently on disk when the plan was built. Applied again
    /// before writing so the restore never clobbers a concurrent edit.
    pub expected_current: Option<ContentDigest>,
    /// Original bytes captured before the turn (empty for `Delete`).
    pub snapshot: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreConflictReason {
    ExternalModified,
    CheckpointMissing,
    CheckpointUnreadable,
    FileReadFailed,
    PathUnsupported,
    Uncertain,
}

impl RestoreConflictReason {
    pub fn label(self) -> &'static str {
        match self {
            RestoreConflictReason::ExternalModified => "文件已被其他程序修改",
            RestoreConflictReason::CheckpointMissing => "缺少检查点快照",
            RestoreConflictReason::CheckpointUnreadable => "无法读取检查点快照",
            RestoreConflictReason::FileReadFailed => "无法读取当前文件",
            RestoreConflictReason::PathUnsupported => "路径类型不支持安全还原",
            RestoreConflictReason::Uncertain => "写入未完成，状态无法确认",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RestoreConflict {
    pub path: String,
    pub reason: RestoreConflictReason,
    pub operations: u32,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RestorePlan {
    pub removed_messages: usize,
    /// Number of visible user turns starting at the edited message.
    pub removed_user_turns: usize,
    pub files: Vec<RestoreFile>,
    pub conflicts: Vec<RestoreConflict>,
    /// Shell commands whose file effects cannot be tracked.
    pub shell_commands: Vec<String>,
}

impl RestorePlan {
    pub fn can_restore(&self) -> bool {
        self.conflicts.is_empty()
    }

    pub fn has_file_changes(&self) -> bool {
        !self.files.is_empty() || !self.conflicts.is_empty()
    }
}

#[derive(Default)]
struct AccumulatedFile {
    path: String,
    earliest_turn: String,
    earliest_index: usize,
    earliest: Option<FileCheckpoint>,
    latest_index: usize,
    expected_after: Option<ContentDigest>,
    operations: u32,
    tool_names: Vec<String>,
    uncertain: bool,
}

/// Build the restore plan for editing/undoing `message_id`.
///
/// The earliest checkpoint for each path wins: if turn A created a file and
/// turn B edited it again, undoing from A must delete it, not restore B's
/// intermediate content.
pub(crate) fn plan_restore(
    session: &SessionState,
    message_id: &str,
    root: &Path,
) -> Result<RestorePlan, String> {
    plan_restore_in(session, message_id, root, &[])
}

/// [`plan_restore`] with the project's additional folders. Checkpoint paths
/// for files outside the session cwd are stored as absolute paths (the tools'
/// display path); they are accepted only inside one of `allowed_roots`.
pub(crate) fn plan_restore_in(
    session: &SessionState,
    message_id: &str,
    root: &Path,
    allowed_roots: &[PathBuf],
) -> Result<RestorePlan, String> {
    let cut_index = session
        .message_position(message_id)
        .ok_or_else(|| "消息已不在当前会话中。".to_string())?;
    let message = session
        .messages
        .get(cut_index)
        .ok_or_else(|| "消息已不在当前会话中。".to_string())?;
    if message.role != effibuddy_agent_core::Role::User
        || message.synthetic
        || message.agent_message.is_some()
    {
        return Err("只能编辑或撤销用户输入的消息。".to_string());
    }
    if message.mid_turn {
        // Steered messages ride inside a checkpoint owned by the prompt that
        // started the run; rewinding one would target a prefix it never owned.
        return Err("回合中插入的消息不支持编辑或撤销。".to_string());
    }

    let mut order: HashMap<&str, usize> = HashMap::new();
    for (index, message) in session.messages.iter().enumerate() {
        order.insert(message.id.as_str(), index);
    }
    // Tool paths are relative to the workspace's canonical root. Resolve the
    // session cwd the same way so a workspace opened through a symlink still
    // points at the files the tools wrote.
    let workspace = fs::canonicalize(&session.cwd).unwrap_or_else(|_| session.cwd.clone());

    let mut accumulated: BTreeMap<String, AccumulatedFile> = BTreeMap::new();
    let mut shell_commands: Vec<String> = Vec::new();

    for checkpoint in &session.checkpoints {
        let Some(&index) = order.get(checkpoint.message_id.as_str()) else {
            continue;
        };
        if index < cut_index {
            continue;
        }
        for command in &checkpoint.shell_commands {
            if !shell_commands.iter().any(|existing| existing == command) {
                shell_commands.push(command.clone());
            }
        }
        for file in &checkpoint.files {
            let entry = accumulated
                .entry(file.path.clone())
                .or_insert_with(|| AccumulatedFile {
                    path: file.path.clone(),
                    earliest_turn: checkpoint.message_id.clone(),
                    earliest_index: index,
                    earliest: Some(file.clone()),
                    latest_index: index,
                    expected_after: file.expected_after,
                    operations: 0,
                    tool_names: Vec::new(),
                    uncertain: false,
                });
            if index < entry.earliest_index {
                entry.earliest_index = index;
                entry.earliest_turn = checkpoint.message_id.clone();
                entry.earliest = Some(file.clone());
            }
            if index >= entry.latest_index {
                entry.latest_index = index;
                entry.expected_after = file.expected_after;
            }
            entry.operations = entry.operations.saturating_add(file.operation_count);
            for tool in &file.tool_names {
                if !entry.tool_names.iter().any(|existing| existing == tool) {
                    entry.tool_names.push(tool.clone());
                }
            }
            entry.uncertain |= file.uncertain;
        }
    }

    let removed_user_turns = session
        .messages
        .iter()
        .skip(cut_index)
        .filter(|message| {
            message.role == effibuddy_agent_core::Role::User
                && !message.synthetic
                && !message.mid_turn
                && message.agent_message.is_none()
        })
        .count();
    let mut plan = RestorePlan {
        removed_messages: session.messages.len().saturating_sub(cut_index),
        removed_user_turns,
        files: Vec::new(),
        conflicts: Vec::new(),
        shell_commands,
    };

    for (_, entry) in accumulated {
        let absolute = workspace.join(&entry.path);
        // Files in a project's additional folders carry an absolute path. A
        // restore must never write outside the session cwd or one of the
        // current project roots, even if the transcript was edited by hand.
        let escaped = absolute.is_absolute()
            && !absolute.starts_with(&workspace)
            && !allowed_roots.iter().any(|allowed| {
                let allowed = fs::canonicalize(allowed).unwrap_or_else(|_| allowed.clone());
                absolute.starts_with(&allowed)
            });
        let metadata = fs::symlink_metadata(&absolute);
        let (current_digest, current_unsupported, unsupported_reason) = if escaped {
            (None, true, Some(RestoreConflictReason::PathUnsupported))
        } else {
            match &metadata {
                Ok(meta) if meta.is_symlink() => {
                    (None, true, Some(RestoreConflictReason::PathUnsupported))
                }
                Ok(meta) if meta.is_file() => {
                    if meta.len() > MAX_SNAPSHOT_BYTES {
                        // Do not pull an unbounded file into memory just to hash
                        // it; report it as unreadable and leave it untouched.
                        (None, true, Some(RestoreConflictReason::FileReadFailed))
                    } else {
                        match fs::read(&absolute) {
                            Ok(bytes) => (Some(ContentDigest::of(&bytes)), false, None),
                            Err(_) => (None, true, Some(RestoreConflictReason::FileReadFailed)),
                        }
                    }
                }
                Ok(meta) if meta.is_dir() => {
                    (None, true, Some(RestoreConflictReason::PathUnsupported))
                }
                Ok(_) => (None, true, Some(RestoreConflictReason::PathUnsupported)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (None, false, None),
                Err(_) => (None, true, Some(RestoreConflictReason::FileReadFailed)),
            }
        };

        let mut conflict = entry.uncertain.then_some(RestoreConflictReason::Uncertain);
        let mut snapshot = Vec::new();

        if conflict.is_none() && current_unsupported {
            conflict = unsupported_reason;
        }

        let earliest = entry.earliest.clone();
        if conflict.is_none()
            && let Some(earliest) = &earliest
            && earliest.existed_before
        {
            match &earliest.snapshot {
                Some(name) => {
                    let path = turn_dir(root, &session.id, &entry.earliest_turn).join(name);
                    match fs::metadata(&path) {
                        Ok(meta) if meta.len() > MAX_SNAPSHOT_BYTES => {
                            conflict = Some(RestoreConflictReason::CheckpointUnreadable);
                        }
                        Ok(_) => match fs::read(&path) {
                            Ok(bytes) => snapshot = bytes,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                conflict = Some(RestoreConflictReason::CheckpointMissing);
                            }
                            Err(_) => {
                                conflict = Some(RestoreConflictReason::CheckpointUnreadable);
                            }
                        },
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            conflict = Some(RestoreConflictReason::CheckpointMissing);
                        }
                        Err(_) => {
                            conflict = Some(RestoreConflictReason::CheckpointUnreadable);
                        }
                    }
                }
                None => conflict = Some(RestoreConflictReason::CheckpointMissing),
            }
        }

        if conflict.is_none() {
            match entry.expected_after {
                Some(expected) if current_digest == Some(expected) => {}
                Some(_) => conflict = Some(RestoreConflictReason::ExternalModified),
                None => conflict = Some(RestoreConflictReason::Uncertain),
            }
        }

        if let Some(reason) = conflict {
            plan.conflicts.push(RestoreConflict {
                path: entry.path,
                reason,
                operations: entry.operations,
                tool_names: entry.tool_names,
            });
            continue;
        }

        let existed_before = earliest
            .as_ref()
            .map(|file| file.existed_before)
            .unwrap_or(false);
        plan.files.push(RestoreFile {
            path: entry.path,
            action: if existed_before {
                RestoreAction::Restore
            } else {
                RestoreAction::Delete
            },
            operations: entry.operations,
            tool_names: entry.tool_names,
            expected_current: current_digest,
            snapshot,
        });
    }

    plan.files.sort_by(|left, right| left.path.cmp(&right.path));
    plan.conflicts
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(plan)
}

/// Restore every file in `plan` and return how many paths were written.
///
/// All files are revalidated before the first write. If anything changed since
/// the preview, nothing is written and the error names the file.
pub(crate) fn apply_restore_at(plan: &RestorePlan, workspace: &Path) -> Result<usize, String> {
    if !plan.can_restore() {
        return Err("存在无法安全还原的文件，未写入任何文件。".to_string());
    }
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let workspace = workspace.as_path();

    for file in &plan.files {
        let absolute = workspace.join(&file.path);
        let current = match fs::symlink_metadata(&absolute) {
            Ok(meta) if meta.is_symlink() || meta.is_dir() => {
                return Err(format!(
                    "`{}` 在检查后变成了不支持的路径类型，未写入任何文件。",
                    file.path
                ));
            }
            Ok(meta) if meta.len() > MAX_SNAPSHOT_BYTES => {
                return Err(format!(
                    "`{}` 文件过大，无法安全校验，未写入任何文件。",
                    file.path
                ));
            }
            Ok(_) => match fs::read(&absolute) {
                Ok(bytes) => Some(ContentDigest::of(&bytes)),
                Err(error) => {
                    return Err(format!(
                        "无法重新读取 `{}`：{error}。未写入任何文件。",
                        file.path
                    ));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "无法读取 `{}`：{error}。未写入任何文件。",
                    file.path
                ));
            }
        };
        if current != file.expected_current {
            return Err(format!(
                "`{}` 在检查后被其他程序修改，未写入任何文件。",
                file.path
            ));
        }
    }

    let mut restored = 0usize;
    for file in &plan.files {
        let absolute = workspace.join(&file.path);
        match file.action {
            RestoreAction::Restore => {
                atomic_write(&absolute, &file.snapshot)
                    .map_err(|error| format!("无法还原 `{}`：{error}", file.path))?;
                restored += 1;
            }
            RestoreAction::Delete => {
                if absolute.exists() {
                    fs::remove_file(&absolute)
                        .map_err(|error| format!("无法删除 `{}`：{error}", file.path))?;
                }
                restored += 1;
            }
        }
    }
    Ok(restored)
}

/// Drop checkpoint metadata and snapshot blobs for turns that were trimmed out
/// of the transcript.
pub(crate) fn drop_turn_checkpoints(
    session: &mut SessionState,
    message_ids: &[String],
    root: &Path,
) {
    for message_id in message_ids {
        session.remove_checkpoint(message_id);
        let _ = fs::remove_dir_all(turn_dir(root, &session.id, message_id));
    }
}

/// Delete every checkpoint for a session (used when a transcript is deleted).
pub(crate) fn remove_session_checkpoints(session_id: &str) {
    let _ = fs::remove_dir_all(session_dir(&checkpoint_root(), session_id));
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_extension(format!("effibuddy-{}-{stamp}.tmp", std::process::id()));
    let permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
    fs::write(&temporary, bytes)?;
    if let Some(permissions) = permissions {
        // Rename replaces the inode; carry the original mode over so an
        // executable script stays executable after a rewind.
        let _ = fs::set_permissions(&temporary, permissions);
    }
    #[cfg(windows)]
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::Message;
    use std::sync::Arc;

    fn user_session(workspace: &Path) -> (SharedSession, String) {
        let mut session = SessionState::new(workspace.to_path_buf(), "test-model");
        let message = Message::user("first prompt");
        let id = message.id.clone();
        session.push_message(message);
        (Arc::new(Mutex::new(session)), id)
    }

    fn recorder(session: &SharedSession, root: &Path) -> CheckpointRecorder {
        CheckpointRecorder::with_root(session.clone(), root.to_path_buf())
    }

    #[test]
    fn records_before_state_and_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("src").join("lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"old").unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);

        sink.before_write("src/lib.rs", &file, "write_file");
        fs::write(&file, b"new").unwrap();
        sink.after_write("src/lib.rs", &file, "write_file", b"new");

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].action, RestoreAction::Restore);
        assert_eq!(plan.files[0].snapshot, b"old");
        assert_eq!(apply_restore_at(&plan, &workspace).unwrap(), 1);
        assert_eq!(fs::read(&file).unwrap(), b"old");
    }

    #[test]
    fn external_modification_blocks_restore() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, b"old").unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);

        sink.before_write("a.txt", &file, "write_file");
        fs::write(&file, b"agent").unwrap();
        sink.after_write("a.txt", &file, "write_file", b"agent");
        fs::write(&file, b"external").unwrap();

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert!(plan.files.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(
            plan.conflicts[0].reason,
            RestoreConflictReason::ExternalModified
        );
        assert!(apply_restore_at(&plan, &workspace).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"external");
    }

    #[test]
    fn earliest_turn_wins_when_the_same_file_is_edited_twice() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("src.txt");
        fs::write(&file, b"v0").unwrap();
        let (session, first_id) = user_session(&workspace);

        let first = recorder(&session, &snapshots);
        first.before_write("src.txt", &file, "write_file");
        fs::write(&file, b"v1").unwrap();
        first.after_write("src.txt", &file, "write_file", b"v1");

        {
            let mut session = lock_session(&session);
            session.push_message(Message::assistant());
            session.push_message(Message::user("second prompt"));
        }
        let second = recorder(&session, &snapshots);
        second.before_write("src.txt", &file, "write_file");
        fs::write(&file, b"v2").unwrap();
        second.after_write("src.txt", &file, "write_file", b"v2");

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, &first_id, &snapshots).unwrap()
        };
        assert_eq!(plan.removed_user_turns, 2);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].action, RestoreAction::Restore);
        assert_eq!(plan.files[0].operations, 2);
        assert_eq!(plan.files[0].snapshot, b"v0");
        apply_restore_at(&plan, &workspace).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"v0");
    }

    #[test]
    fn removing_a_created_file_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("new.txt");
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);

        sink.before_write("new.txt", &file, "write_file");
        fs::write(&file, b"created").unwrap();
        sink.after_write("new.txt", &file, "write_file", b"created");

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].action, RestoreAction::Delete);
        apply_restore_at(&plan, &workspace).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn shell_commands_are_reported_as_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);
        sink.note_shell("cargo fmt");

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert_eq!(plan.shell_commands, vec!["cargo fmt".to_string()]);
        assert!(plan.files.is_empty());
        assert!(plan.can_restore());
    }

    #[test]
    fn missing_snapshot_is_reported_and_blocks_restore() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, b"old").unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);
        sink.before_write("a.txt", &file, "write_file");
        fs::write(&file, b"agent").unwrap();
        sink.after_write("a.txt", &file, "write_file", b"agent");

        let session_id = lock_session(&session).id.clone();
        fs::remove_dir_all(snapshots.join(&session_id)).unwrap();

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert!(plan.files.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(
            plan.conflicts[0].reason,
            RestoreConflictReason::CheckpointMissing
        );
        assert!(apply_restore_at(&plan, &workspace).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"agent");
    }

    #[test]
    fn one_external_change_blocks_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let safe = workspace.join("safe.txt");
        let external = workspace.join("external.txt");
        fs::write(&safe, b"safe-old").unwrap();
        fs::write(&external, b"external-old").unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);

        sink.before_write("safe.txt", &safe, "write_file");
        fs::write(&safe, b"safe-agent").unwrap();
        sink.after_write("safe.txt", &safe, "write_file", b"safe-agent");
        sink.before_write("external.txt", &external, "write_file");
        fs::write(&external, b"external-agent").unwrap();
        sink.after_write("external.txt", &external, "write_file", b"external-agent");
        fs::write(&external, b"external-user").unwrap();

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.conflicts.len(), 1);
        assert!(!plan.can_restore());
        assert!(apply_restore_at(&plan, &workspace).is_err());
        // The safe file is deliberately left untouched: the dialog promises
        // that a conflict means no file is written.
        assert_eq!(fs::read(&safe).unwrap(), b"safe-agent");
        assert_eq!(fs::read(&external).unwrap(), b"external-user");
    }

    #[test]
    fn apply_aborts_when_a_file_changes_after_preview() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let snapshots = dir.path().join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, b"old").unwrap();
        let (session, _) = user_session(&workspace);
        let sink = recorder(&session, &snapshots);
        sink.before_write("a.txt", &file, "write_file");
        fs::write(&file, b"agent").unwrap();
        sink.after_write("a.txt", &file, "write_file", b"agent");

        let plan = {
            let session = lock_session(&session);
            plan_restore(&session, session.messages[0].id.as_str(), &snapshots).unwrap()
        };
        fs::write(&file, b"race").unwrap();
        let error = apply_restore_at(&plan, &workspace).unwrap_err();
        assert!(error.contains("检查后被其他程序修改"), "{error}");
        assert_eq!(fs::read(&file).unwrap(), b"race");
    }

    #[test]
    fn plan_requires_a_real_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let mut session = SessionState::new(workspace.clone(), "test-model");
        let synthetic = Message::synthetic_user("notification");
        let id = synthetic.id.clone();
        session.push_message(synthetic);
        assert!(plan_restore(&session, &id, dir.path()).is_err());
    }

    #[test]
    fn steered_messages_are_not_rewindable() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let mut session = SessionState::new(workspace, "test-model");
        let steered = Message::mid_turn_user("steer");
        let id = steered.id.clone();
        session.push_message(steered);
        let error = plan_restore(&session, &id, dir.path()).unwrap_err();
        assert!(error.contains("回合中插入"), "{error}");
    }

    #[test]
    fn truncate_drops_compaction_when_the_prefix_is_removed() {
        let mut session = SessionState::new(PathBuf::from("/tmp"), "m");
        session.push_message(Message::user("one"));
        session.push_message(Message::assistant());
        session.compacted_upto = 2;
        session.truncate_from(0);
        assert!(session.messages.is_empty());
        assert_eq!(session.compacted_upto, 0);
        assert!(session.compaction.is_none());
    }

    #[test]
    fn recorder_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckpointRecorder>();
    }

    #[test]
    fn session_roundtrips_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (session, id) = user_session(dir.path());
        {
            let mut session = lock_session(&session);
            let checkpoint = session.checkpoint_or_insert(&id);
            checkpoint.files.push(FileCheckpoint {
                path: "a.txt".into(),
                existed_before: true,
                snapshot: Some("0001.bin".into()),
                expected_after: Some(ContentDigest::of(b"x")),
                operation_count: 1,
                tool_names: vec!["write_file".into()],
                uncertain: false,
            });
        }
        let json = serde_json::to_string(&*lock_session(&session)).unwrap();
        let restored: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.checkpoints.len(), 1);
        assert_eq!(restored.checkpoints[0].files[0].path, "a.txt");
        assert_eq!(
            restored.checkpoints[0].files[0].expected_after,
            Some(ContentDigest::of(b"x"))
        );
    }

    /// Manual performance lab. Run with:
    /// `cargo test --release -p effibuddy-app --bin effibuddy
    ///  checkpoint_perf_lab -- --ignored --nocapture`
    ///
    /// Env knobs: `EFFI_PERF_FILES` (default 20), `EFFI_PERF_BYTES`
    /// (default 64 KiB), `EFFI_PERF_TURNS` (default 64 metadata turns).
    #[test]
    #[ignore = "manual performance lab"]
    fn checkpoint_perf_lab() {
        use std::hint::black_box;
        use std::time::Instant;

        let files: usize = std::env::var("EFFI_PERF_FILES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(20);
        let bytes: usize = std::env::var("EFFI_PERF_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(64 * 1024);
        let turns: usize = std::env::var("EFFI_PERF_TURNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(64);

        // `EFFI_PERF_ROOT` targets another filesystem (e.g. the mounted
        // workspace) instead of the temp dir; its contents get wiped.
        let temp = tempfile::tempdir().unwrap();
        let root = match std::env::var("EFFI_PERF_ROOT") {
            Ok(path) => {
                let path = PathBuf::from(path);
                let _ = fs::remove_dir_all(&path);
                fs::create_dir_all(&path).unwrap();
                path
            }
            Err(_) => temp.path().to_path_buf(),
        };
        let workspace = root.join("ws");
        let snapshots = root.join("checkpoints");
        fs::create_dir_all(&workspace).unwrap();

        let old = vec![b'o'; bytes];
        let new = vec![b'n'; bytes];
        let paths: Vec<(String, PathBuf)> = (0..files)
            .map(|index| {
                let relative = format!("src/f{index}.txt");
                let absolute = workspace.join(&relative);
                fs::create_dir_all(absolute.parent().unwrap()).unwrap();
                fs::write(&absolute, &old).unwrap();
                (relative, absolute)
            })
            .collect();

        // Baseline: the same writes without any checkpoint work.
        let started = Instant::now();
        for (_, absolute) in &paths {
            fs::write(absolute, &new).unwrap();
        }
        let baseline = started.elapsed();
        for (_, absolute) in &paths {
            fs::write(absolute, &old).unwrap();
        }

        let mut session = SessionState::new(workspace.clone(), "perf-model");
        let prompt = Message::user("perf");
        let turn_id = prompt.id.clone();
        session.push_message(prompt);
        let shared = Arc::new(Mutex::new(session));
        let sink = CheckpointRecorder::with_root(shared.clone(), snapshots.clone());

        // First write of each path: before-snapshot + write + digest.
        let started = Instant::now();
        for (relative, absolute) in &paths {
            sink.before_write(relative, absolute, "write_file");
            fs::write(absolute, &new).unwrap();
            sink.after_write(relative, absolute, "write_file", &new);
        }
        let first = started.elapsed();

        // Repeat writes: before-write fast path + write + digest update.
        let started = Instant::now();
        for (relative, absolute) in &paths {
            sink.before_write(relative, absolute, "write_file");
            fs::write(absolute, &new).unwrap();
            sink.after_write(relative, absolute, "write_file", &new);
        }
        let repeat = started.elapsed();

        let started = Instant::now();
        let plan = {
            let session = lock_session(&shared);
            plan_restore(&session, &turn_id, &snapshots).unwrap()
        };
        let plan_time = started.elapsed();
        assert_eq!(plan.files.len(), files);
        assert!(plan.can_restore());

        let started = Instant::now();
        let restored = apply_restore_at(&plan, &workspace).unwrap();
        let apply_time = started.elapsed();
        assert_eq!(restored, files);

        // Metadata cost at full history: `turns` copies of this turn's table.
        let metadata_len = {
            let mut session = lock_session(&shared);
            let template = session.checkpoints.first().cloned().expect("checkpoint");
            session.checkpoints.clear();
            for index in 0..turns {
                let mut checkpoint = template.clone();
                checkpoint.message_id = format!("perf-turn-{index}");
                session.checkpoints.push(checkpoint);
            }
            let json = serde_json::to_string_pretty(&*session).unwrap();
            black_box(json.len())
        };
        let started = Instant::now();
        {
            let session = lock_session(&shared);
            let json = serde_json::to_string_pretty(&*session).unwrap();
            black_box(json);
        }
        let metadata_serialize = started.elapsed();

        // Persistence cost. Old path: `store.save` (serialize + write) then
        // `store.stage` (serialize again). New path: stage once, write that
        // payload synchronously, then enqueue the same payload.
        let store = crate::store::SessionStore::with_directory(root.join("sessions"));
        let snapshot = { lock_session(&shared).clone() };
        let started = Instant::now();
        store.save(&snapshot).unwrap();
        let old_save = started.elapsed();
        let started = Instant::now();
        let staged = store.stage(&snapshot).unwrap();
        let stage_time = started.elapsed();
        let old_save_active = old_save + stage_time;
        black_box(&staged);
        let started = Instant::now();
        staged.write().unwrap();
        let single_write = started.elapsed();
        let new_save_active = stage_time + single_write;
        drop(staged);

        let total_mib = (files * bytes) as f64 / (1024.0 * 1024.0);
        let rate = |elapsed: std::time::Duration| {
            if elapsed.as_secs_f64() > 0.0 {
                total_mib / elapsed.as_secs_f64()
            } else {
                f64::INFINITY
            }
        };
        println!("\n== checkpoint perf ==");
        println!(
            "files={files} bytes={bytes} total={total_mib:.2} MiB metadata_turns={turns} \
             metadata_json={:.1} KiB",
            metadata_len as f64 / 1024.0
        );
        println!(
            "baseline write         {:>10.2?}  {:>8.1} MiB/s",
            baseline,
            rate(baseline)
        );
        println!(
            "first write + snapshot {:>10.2?}  {:>8.1} MiB/s  ({:.2}x)",
            first,
            rate(first),
            first.as_secs_f64() / baseline.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        println!(
            "repeat write           {:>10.2?}  {:>8.1} MiB/s  ({:.2}x)",
            repeat,
            rate(repeat),
            repeat.as_secs_f64() / baseline.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        println!(
            "plan_restore           {:>10.2?}  (reads current + snapshots, hashes both)",
            plan_time
        );
        println!("apply_restore          {:>10.2?}", apply_time);
        println!(
            "metadata pretty json   {:>10.2?}  ({:.1} MiB)",
            metadata_serialize,
            metadata_len as f64 / (1024.0 * 1024.0)
        );
        println!(
            "store.save (ser+write) {:>10.2?}\nstore.stage (serialize) {:>10.2?}\n\
             old save_active (2x ser) {:>9.2?}\n\
             stage.write (ser+write) {:>9.2?}\n\
             new save_active (1x ser) {:>9.2?}",
            old_save, stage_time, old_save_active, single_write, new_save_active
        );
        println!(
            "per-file first write: {:.2?}\n",
            first / files.max(1) as u32
        );
    }
}
