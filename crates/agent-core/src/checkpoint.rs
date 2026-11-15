//! File checkpoints for conversation rewind.
//!
//! ZCode snapshots the workspace through private Git refs. EffiBuddy keeps the
//! mechanism deliberately VCS-free: file tools report the state of each file
//! *before* they mutate it, the host stores the original bytes in its data
//! directory, and a rewind is a verified filesystem restore. Nothing in this
//! module invokes Git or creates repository state.
//!
//! The transcript owns one [`TurnCheckpoint`] per user message. Background
//! notifications and subagent writes that belong to the same user turn append
//! to that checkpoint, so editing or undoing the user message can restore
//! everything the turn changed.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stable content identity used to detect external edits between the agent's
/// last write and a rewind request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDigest {
    /// FNV-1a over the bytes. Deterministic across processes and platforms.
    pub hash: u64,
    pub len: u64,
}

impl ContentDigest {
    pub fn of(bytes: &[u8]) -> Self {
        // FNV-1a 64-bit: tiny, dependency-free, and stable across runs.
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        Self {
            hash,
            len: bytes.len() as u64,
        }
    }
}

/// One file touched during a user turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCheckpoint {
    /// Workspace-relative path using `/` separators.
    pub path: String,
    /// Whether the file existed when the first write of this turn started.
    pub existed_before: bool,
    /// Snapshot file name inside the turn's checkpoint directory. `None` when
    /// the file did not exist before the turn or the snapshot failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Digest of the content written by the most recent successful tool call
    /// in this turn. Rewind preflight compares the current file against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_after: Option<ContentDigest>,
    #[serde(default)]
    pub operation_count: u32,
    #[serde(default)]
    pub tool_names: Vec<String>,
    /// A write failed midway, or the snapshot could not be produced. Rewind
    /// refuses to touch such a file.
    #[serde(default)]
    pub uncertain: bool,
}

/// Everything one user turn changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCheckpoint {
    /// Id of the user message that started the turn. Runs woken by background
    /// notifications append to the same checkpoint.
    pub message_id: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub files: Vec<FileCheckpoint>,
    /// Shell commands executed in the turn. Their file effects cannot be
    /// tracked, so the rewind UI lists them as ignored changes.
    #[serde(default)]
    pub shell_commands: Vec<String>,
}

impl TurnCheckpoint {
    pub fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            created_at: Utc::now(),
            files: Vec::new(),
            shell_commands: Vec::new(),
        }
    }

    pub fn file_mut(&mut self, path: &str) -> Option<&mut FileCheckpoint> {
        self.files.iter_mut().find(|file| file.path == path)
    }
}

/// Host callback used by mutating tools.
///
/// Implementations keep all persistence and path resolution on the host side;
/// tools only report what they are about to do. Calls must be cheap and must
/// not panic: a broken checkpoint recorder must never take down a run.
pub trait FileCheckpointSink: Send + Sync {
    /// Called immediately before a tool writes `absolute_path`.
    fn before_write(&self, relative_path: &str, absolute_path: &Path, tool: &str);

    /// Called after a successful write with the exact bytes written.
    fn after_write(&self, relative_path: &str, absolute_path: &Path, tool: &str, content: &[u8]);

    /// Called when a write returned an error. The file may be partially
    /// written, so the recorder marks it unrecoverable.
    fn write_failed(&self, relative_path: &str, absolute_path: &Path, tool: &str);

    /// Called before a shell command runs. Commands can modify arbitrary
    /// files, so the turn is flagged as having untracked changes.
    fn note_shell(&self, command: &str);
}

/// Shared handle stored in tool contexts and harness configs.
pub type SharedFileCheckpointSink = std::sync::Arc<dyn FileCheckpointSink>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_length_sensitive() {
        let a = ContentDigest::of(b"hello");
        let b = ContentDigest::of(b"hello");
        let c = ContentDigest::of(b"hello!");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len, 5);
    }
}
