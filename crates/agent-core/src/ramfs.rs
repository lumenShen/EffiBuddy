//! RAM-backed workspaces for the stress lab.
//!
//! The stress lab must never write to persistent storage: heavy load would
//! wear out the SSD for no benefit. Instead of a bespoke in-memory filesystem
//! abstraction at every write site, the lab swaps the *directory* behind the
//! existing filesystem interface to a tmpfs location:
//!
//! - Linux: `/dev/shm/effibuddy-stress-…` (tmpfs — kernel RAM, identical
//!   syscall semantics, atomic renames keep working);
//! - other platforms: the OS temp directory (best effort, documented).
//!
//! A [`RamDir`] removes its tree when released *and* on drop, so pages go
//! back to the kernel promptly even when a run panics or the host forgets to
//! call [`RamDir::release`].

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PREFIX: &str = "effibuddy-stress-";
/// Crash residue older than this is swept on the next acquire.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// A temporary, memory-backed directory tree.
///
/// Not `Clone` on purpose: each handle owns exactly one tree and releases it
/// exactly once (on [`RamDir::release`] or `Drop`).
pub struct RamDir {
    path: PathBuf,
}

impl RamDir {
    /// Create a fresh RAM-backed workspace rooted at
    /// `<tmpfs>/effibuddy-stress-<label>-<uuid>/`.
    ///
    /// `label` should be short and filesystem-safe (a run id or mode name).
    pub fn acquire(label: &str) -> io::Result<Self> {
        let root = tmpfs_root();
        sweep_stale(&root);
        let path = root.join(format!("{PREFIX}{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Join a relative path under this workspace.
    pub fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.path.join(relative)
    }

    /// Delete the whole tree now and return the bytes freed (best effort).
    ///
    /// Takes `&self` on purpose: stress runners share the handle through
    /// `Arc`. Dropping the last handle has the same effect; this exists so
    /// the runner can report the release explicitly (and idempotently).
    pub fn release(&self) -> u64 {
        drop_inner(self.path.clone())
    }
}

impl Drop for RamDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::fmt::Debug for RamDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamDir").field("path", &self.path).finish()
    }
}

fn drop_inner(path: PathBuf) -> u64 {
    let freed = dir_size(&path);
    let _ = std::fs::remove_dir_all(&path);
    freed
}

/// The memory-backed parent directory. `/dev/shm` on Linux (tmpfs by
/// convention), the OS temp dir elsewhere.
fn tmpfs_root() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let shm = PathBuf::from("/dev/shm");
        if shm.is_dir() {
            return shm;
        }
    }
    std::env::temp_dir()
}

/// Remove leftover stress trees from crashed runs (older than [`STALE_AFTER`]).
fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(STALE_AFTER);
        if age >= STALE_AFTER {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_join_release_roundtrip() {
        let dir = RamDir::acquire("test").expect("acquire");
        assert!(dir.path().is_dir());
        assert!(dir.path().to_string_lossy().contains(PREFIX));

        let file = dir.join("sessions/demo.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{}").unwrap();
        assert!(file.exists());

        let path = dir.path().to_path_buf();
        let freed = dir.release();
        assert!(freed > 0);
        assert!(!path.exists(), "directory must be gone after release");
    }

    #[test]
    fn drop_releases_the_tree() {
        let dir = RamDir::acquire("drop-test").expect("acquire");
        let path = dir.path().to_path_buf();
        std::fs::write(dir.join("x.txt"), "hello").unwrap();
        drop(dir);
        assert!(!path.exists(), "directory must be gone after drop");
    }
}
