//! Workspace-relative path resolution.
//!
//! Built-in file tools refuse paths that escape the workspace roots. A project
//! may own several folders: the primary root anchors relative paths (shell
//! cwd, file tree, git) while additional roots stay readable and writable for
//! the agent. This is a convenience boundary for well-behaved agents, not a
//! kernel-level sandbox: shell commands and process APIs can still reach the
//! rest of the machine (and therefore require explicit approval).

use std::path::{Component, Path, PathBuf};

use effibuddy_agent_core::ToolError;

#[derive(Clone, Debug)]
struct Root {
    root: PathBuf,
    canonical_root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    /// Primary root first; additional project folders follow in project order.
    roots: Vec<Root>,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        Self::with_roots(root, &[])
    }

    /// Primary root plus additional project folders. Unavailable additional
    /// folders are skipped so a moved or deleted folder never prevents a run;
    /// the primary root is still required.
    pub fn with_roots(
        primary: impl Into<PathBuf>,
        additional: &[PathBuf],
    ) -> Result<Self, ToolError> {
        let primary = primary.into();
        let canonical_root = std::fs::canonicalize(&primary).map_err(|error| {
            ToolError::new(format!(
                "workspace `{}` is not accessible: {error}",
                primary.display()
            ))
        })?;
        let mut roots = vec![Root {
            root: primary,
            canonical_root,
        }];
        for folder in additional {
            let Ok(canonical_root) = std::fs::canonicalize(folder) else {
                continue;
            };
            if roots
                .iter()
                .any(|root| root.canonical_root == canonical_root)
            {
                continue;
            }
            roots.push(Root {
                root: folder.clone(),
                canonical_root,
            });
        }
        Ok(Self { roots })
    }

    /// Primary root; relative tool paths, shell cwd and the file tree use it.
    pub fn root(&self) -> &Path {
        &self.roots[0].root
    }

    pub fn canonical_root(&self) -> &Path {
        &self.roots[0].canonical_root
    }

    /// Every authorized root, primary first (raw paths as configured).
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.root.as_path())
    }

    pub fn is_multi_root(&self) -> bool {
        self.roots.len() > 1
    }

    /// Resolve an existing file or directory and verify it is inside one of
    /// the roots.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::new("path must not be empty"));
        }
        let candidate = self.absolute(raw);
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            ToolError::new(format!("cannot access `{}`: {error}", candidate.display()))
        })?;
        if !self.is_inside(&canonical) {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace roots",
                raw
            )));
        }
        Ok(canonical)
    }

    /// Resolve a path that may not exist yet, verifying its nearest existing
    /// ancestor is inside one of the roots.
    pub fn resolve_for_write(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::new("path must not be empty"));
        }
        let candidate = normalize_lexical(&self.absolute(raw));
        if !candidate.is_absolute() {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace roots",
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
        if !self.is_inside(&canonical_ancestor) {
            return Err(ToolError::new(format!(
                "path `{}` escapes the workspace roots",
                raw
            )));
        }
        let remainder = candidate.strip_prefix(ancestor).unwrap_or(Path::new(""));
        Ok(canonical_ancestor.join(remainder))
    }

    /// Render a path for display: relative for the primary root (unchanged
    /// behavior), absolute for additional roots so the model can tell two
    /// same-named files apart.
    pub fn display_path(&self, path: &Path) -> String {
        for (index, root) in self.roots.iter().enumerate() {
            let relative = path
                .strip_prefix(&root.canonical_root)
                .or_else(|_| path.strip_prefix(&root.root));
            if let Ok(relative) = relative {
                let text = if index == 0 {
                    relative.to_string_lossy().replace('\\', "/")
                } else {
                    path.to_string_lossy().replace('\\', "/")
                };
                return text;
            }
        }
        path.to_string_lossy().replace('\\', "/")
    }

    fn is_inside(&self, canonical: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| canonical.starts_with(&root.canonical_root))
    }

    fn absolute(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.roots[0].root.join(path)
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

    #[test]
    fn additional_roots_are_readable_and_writable() {
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(extra.path().join("nested")).unwrap();
        let workspace =
            Workspace::with_roots(primary.path(), &[extra.path().to_path_buf()]).unwrap();
        assert!(workspace.is_multi_root());

        // Relative paths still anchor at the primary root.
        assert!(workspace.resolve_for_write("only-primary.txt").is_ok());
        // Absolute paths inside an additional root are allowed.
        let target = extra.path().join("nested/extra.txt");
        assert!(
            workspace
                .resolve_for_write(target.to_str().unwrap())
                .is_ok()
        );
        // Neither root escapes the union.
        let outside = tempfile::tempdir().unwrap();
        assert!(
            workspace
                .resolve_for_write(outside.path().join("x.txt").to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn unavailable_additional_root_is_skipped() {
        let primary = tempfile::tempdir().unwrap();
        let missing = primary.path().join("does-not-exist");
        let workspace = Workspace::with_roots(primary.path(), &[missing]).unwrap();
        assert!(!workspace.is_multi_root());
    }

    #[test]
    fn display_keeps_primary_relative_and_extra_absolute() {
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let workspace =
            Workspace::with_roots(primary.path(), &[extra.path().to_path_buf()]).unwrap();

        let primary_file = primary.path().join("src/main.rs");
        std::fs::create_dir_all(primary_file.parent().unwrap()).unwrap();
        std::fs::write(&primary_file, "fn main() {}").unwrap();
        let canonical = std::fs::canonicalize(&primary_file).unwrap();
        assert_eq!(workspace.display_path(&canonical), "src/main.rs");

        let extra_file = extra.path().join("README.md");
        std::fs::write(&extra_file, "hi").unwrap();
        let canonical = std::fs::canonicalize(&extra_file).unwrap();
        assert_eq!(
            workspace.display_path(&canonical),
            extra_file.to_string_lossy().replace('\\', "/")
        );
    }
}
