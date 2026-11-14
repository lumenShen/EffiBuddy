//! Workspace-relative path resolution.
//!
//! Built-in file tools refuse paths that escape the workspace root. This is a
//! convenience boundary for well-behaved agents, not a kernel-level sandbox:
//! shell commands and process APIs can still reach the rest of the machine
//! (and therefore require explicit approval).

use std::path::{Component, Path, PathBuf};

use effibuddy_agent_core::ToolError;

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    canonical_root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let root = root.into();
        let canonical_root = std::fs::canonicalize(&root).map_err(|error| {
            ToolError::new(format!(
                "workspace `{}` is not accessible: {error}",
                root.display()
            ))
        })?;
        Ok(Self {
            root,
            canonical_root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Resolve an existing file or directory and verify it is inside the root.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::new("path must not be empty"));
        }
        let candidate = self.absolute(raw);
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            ToolError::new(format!("cannot access `{}`: {error}", candidate.display()))
        })?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace root",
                raw
            )));
        }
        Ok(canonical)
    }

    /// Resolve a path that may not exist yet, verifying its nearest existing
    /// ancestor is inside the root.
    pub fn resolve_for_write(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::new("path must not be empty"));
        }
        let candidate = normalize_lexical(&self.absolute(raw));
        if !candidate.is_absolute() {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace root",
                raw
            )));
        }

        let mut ancestor = candidate.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(|| {
                ToolError::new(format!("cannot resolve parent directory of `{}`", raw))
            })?;
        }
        let canonical_ancestor = std::fs::canonicalize(ancestor).map_err(|error| {
            ToolError::new(format!("cannot access `{}`: {error}", ancestor.display()))
        })?;
        if !canonical_ancestor.starts_with(&self.canonical_root) {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace root",
                raw
            )));
        }
        let remainder = candidate.strip_prefix(ancestor).unwrap_or(Path::new(""));
        Ok(canonical_ancestor.join(remainder))
    }

    /// Render a path relative to the workspace root for display.
    pub fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.canonical_root)
            .or_else(|_| path.strip_prefix(&self.root))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn absolute(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_parent_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();
        assert!(workspace.resolve_for_write("../escape.txt").is_err());
        assert!(
            workspace
                .resolve_for_write("nested/../../escape.txt")
                .is_err()
        );
        assert!(workspace.resolve_for_write("nested/file.txt").is_ok());
    }

    #[test]
    fn rejects_absolute_paths_outside_root() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();
        let outside = other.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        assert!(
            workspace
                .resolve_existing(outside.to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn normalizes_cur_dir_components() {
        let normalized = normalize_lexical(Path::new("a/./b/../c"));
        assert_eq!(normalized, PathBuf::from("a/c"));
    }
}
