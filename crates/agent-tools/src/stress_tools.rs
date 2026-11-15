//! Virtual tools for the stress lab — the same tool names and argument
//! contracts as the built-ins, executed entirely in RAM.
//!
//! A [`VirtualWorkspace`] is a plain in-memory file map, so a session's
//! `write_file` → `read_file` → `grep` chain stays coherent without touching
//! any filesystem. For runs that must exercise the *real* tool code paths,
//! [`stress_real_tools`] registers the genuine file tools over a RAM-backed
//! ([`effibuddy_agent_core::RamDir`]) workspace — still no persistent-disk
//! writes, and `shell`/`run_js`/`agent` are deliberately excluded so nothing
//! can escape the sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use effibuddy_agent_core::{
    ApprovalPolicy, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};

use crate::args::as_object;
use crate::todo::TodoWriteTool;
use crate::{EditFileTool, ListDirTool, ReadFileTool, Workspace, WriteFileTool};

/// In-memory file map backing every virtual tool.
#[derive(Clone, Default)]
pub struct VirtualWorkspace {
    files: Arc<Mutex<BTreeMap<PathBuf, String>>>,
}

impl VirtualWorkspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a small skeleton so `read_file`/`grep`/`list_dir` have content on
    /// the very first step of every session.
    pub fn seed_skeleton(&self) {
        let mut files = self.lock();
        files.insert(
            PathBuf::from("README.md"),
            "# Virtual workspace\n\nSeeded for the stress lab.\n".into(),
        );
        files.insert(
            PathBuf::from("src/main.rs"),
            "fn main() {\n    println!(\"virtual\");\n}\n".into(),
        );
        files.insert(
            PathBuf::from("src/lib.rs"),
            "pub mod agent {\n    pub const VERSION: &str = \"0.1.0\";\n}\n".into(),
        );
        files.insert(
            PathBuf::from("docs/plan.md"),
            "# Plan\n\n- [x] baseline\n- [ ] concurrency\n- [ ] report\n".into(),
        );
    }

    pub fn write(&self, path: &Path, content: &str) {
        self.lock().insert(path.to_path_buf(), content.to_string());
    }

    pub fn read(&self, path: &Path) -> Option<String> {
        self.lock().get(path).cloned()
    }

    pub fn edit(&self, path: &Path, old: &str, new: &str) -> Option<usize> {
        let mut files = self.lock();
        let content = files.get_mut(path)?;
        let count = content.matches(old).count();
        if count > 0 {
            *content = content.replace(old, new);
        }
        Some(count)
    }

    /// Files under `dir` (`.` or empty → everything).
    pub fn list(&self, dir: &Path) -> Vec<PathBuf> {
        let files = self.lock();
        files
            .keys()
            .filter(|file| starts_with_dir(file, dir))
            .cloned()
            .collect()
    }

    /// `(path, line_no, line)` hits for a literal pattern.
    pub fn grep(
        &self,
        pattern: &str,
        dir: &Path,
        max_results: usize,
    ) -> Vec<(PathBuf, usize, String)> {
        let files = self.lock();
        let mut hits = Vec::new();
        for (path, content) in files.iter() {
            if !starts_with_dir(path, dir) {
                continue;
            }
            for (index, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    hits.push((path.clone(), index + 1, line.to_string()));
                    if hits.len() >= max_results {
                        return hits;
                    }
                }
            }
        }
        hits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, String>> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn starts_with_dir(path: &Path, dir: &Path) -> bool {
    if dir.as_os_str().is_empty() || dir == Path::new(".") {
        return true;
    }
    path.starts_with(dir)
}

fn required_string(arguments: &Value, key: &str) -> Result<String, ToolError> {
    as_object(arguments)?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::new(format!("`{key}` must be a string")))
}

fn optional_string(arguments: &Value, key: &str) -> Result<Option<String>, ToolError> {
    Ok(as_object(arguments)?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn optional_u64(arguments: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    Ok(as_object(arguments)?.get(key).and_then(Value::as_u64))
}

// ---------------------------------------------------------------------------
// Virtual tool implementations. All are `ApprovalPolicy::Auto` so approval
// gates never block a stress run.
// ---------------------------------------------------------------------------

/// `read_file` against the virtual workspace.
#[derive(Clone)]
pub struct VirtualReadFile {
    workspace: VirtualWorkspace,
}

impl VirtualReadFile {
    pub fn new(workspace: VirtualWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for VirtualReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a file from the virtual (RAM) workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let path = required_string(&arguments, "path")?;
        let content = self
            .workspace
            .read(Path::new(&path))
            .ok_or_else(|| ToolError::new(format!("file not found: {path}")))?;
        Ok(ToolOutput::text(content))
    }
}

/// `write_file` against the virtual workspace.
#[derive(Clone)]
pub struct VirtualWriteFile {
    workspace: VirtualWorkspace,
}

impl VirtualWriteFile {
    pub fn new(workspace: VirtualWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for VirtualWriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file inside the virtual (RAM) workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let path = required_string(&arguments, "path")?;
        let content = required_string(&arguments, "content")?;
        let bytes = content.len();
        self.workspace.write(Path::new(&path), &content);
        Ok(ToolOutput::text(format!("wrote {bytes} bytes to {path}")))
    }
}

/// `edit_file` against the virtual workspace.
#[derive(Clone)]
pub struct VirtualEditFile {
    workspace: VirtualWorkspace,
}

impl VirtualEditFile {
    pub fn new(workspace: VirtualWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for VirtualEditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string inside a virtual (RAM) workspace file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let path = required_string(&arguments, "path")?;
        let old = required_string(&arguments, "old_string")?;
        let new = required_string(&arguments, "new_string")?;
        match self.workspace.edit(Path::new(&path), &old, &new) {
            Some(count) if count > 0 => Ok(ToolOutput::text(format!(
                "{count} replacement(s) applied to {path}"
            ))),
            Some(_) => Err(ToolError::new(format!("`old_string` not found in {path}"))),
            None => Err(ToolError::new(format!("file not found: {path}"))),
        }
    }
}

/// `list_dir` against the virtual workspace.
#[derive(Clone)]
pub struct VirtualListDir {
    workspace: VirtualWorkspace,
}

impl VirtualListDir {
    pub fn new(workspace: VirtualWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for VirtualListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files recorded in the virtual (RAM) workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let path = optional_string(&arguments, "path")?.unwrap_or_else(|| ".".to_string());
        let mut entries = self.workspace.list(Path::new(&path));
        entries.sort();
        if entries.is_empty() {
            return Ok(ToolOutput::text(format!("(no files under {path})")));
        }
        let body = entries
            .iter()
            .map(|entry| entry.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(format!(
            "{body}\n({} files)",
            entries.len()
        )))
    }
}

/// `grep` against the virtual workspace (literal pattern, coherent with
/// virtual `write_file`).
#[derive(Clone)]
pub struct VirtualGrep {
    workspace: VirtualWorkspace,
}

impl VirtualGrep {
    pub fn new(workspace: VirtualWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for VirtualGrep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Literal-pattern search over the virtual (RAM) workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 500},
                "count_only": {"type": "boolean"}
            },
            "required": ["pattern"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let pattern = required_string(&arguments, "pattern")?;
        let path = optional_string(&arguments, "path")?.unwrap_or_else(|| ".".to_string());
        let max_results = optional_u64(&arguments, "max_results")?.unwrap_or(100) as usize;
        let count_only = as_object(&arguments)?
            .get("count_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let hits = self
            .workspace
            .grep(&pattern, Path::new(&path), max_results.max(1));
        if count_only {
            let files = hits
                .iter()
                .map(|(path, _, _)| path.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            return Ok(ToolOutput::text(format!(
                "{pattern}: {} matches in {files} files",
                hits.len()
            )));
        }
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!("{pattern}: no matches")));
        }
        let body = hits
            .iter()
            .map(|(path, line, text)| format!("{}:{line}: {text}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(body))
    }
}

/// The default stress tool set: virtual file tools (pure RAM) plus the real
/// in-memory `todo_write`. Never touches the filesystem.
pub fn stress_tools() -> Result<ToolRegistry, ToolError> {
    let workspace = VirtualWorkspace::new();
    workspace.seed_skeleton();
    Ok(virtual_registry(workspace))
}

fn virtual_registry(workspace: VirtualWorkspace) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(VirtualReadFile::new(workspace.clone()));
    registry.register(VirtualWriteFile::new(workspace.clone()));
    registry.register(VirtualEditFile::new(workspace.clone()));
    registry.register(VirtualListDir::new(workspace.clone()));
    registry.register(VirtualGrep::new(workspace.clone()));
    registry.register(TodoWriteTool::new());
    registry
}

/// The "real tools" stress set: genuine file tools rooted at a RAM-backed
/// workspace (typically `RamDir::acquire(..).join("workspace")`), so tool
/// executions exercise the actual filesystem code path without any
/// persistent-disk writes. `shell`/`run_js`/`agent`/`workflow` are excluded —
/// they can escape a path sandbox or spawn real processes.
pub fn stress_real_tools(root: impl Into<PathBuf>) -> Result<ToolRegistry, ToolError> {
    let workspace = Workspace::new(root)?;
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(workspace.clone()));
    registry.register(WriteFileTool::new(workspace.clone()));
    registry.register(EditFileTool::new(workspace.clone()));
    registry.register(ListDirTool::new(workspace.clone()));
    registry.register(crate::GrepTool::new(workspace));
    registry.register(TodoWriteTool::new());
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            additional_workspaces: Vec::new(),
            session_id: "stress".into(),
            agent_id: "main".into(),
            agent_name: None,
            agent_depth: 0,
            call_id: "call_test".into(),
            cancel: CancellationToken::new(),
            nested: None,
            subagents: None,
            checkpoint_sink: None,
        }
    }

    #[tokio::test]
    async fn write_read_grep_roundtrip_is_coherent() {
        let registry = stress_tools().expect("registry");
        let write = registry.get("write_file").expect("tool");
        write
            .execute(
                ctx(),
                json!({"path": "src/generated.rs", "content": "pub fn stress_marker() {}\n"}),
            )
            .await
            .expect("write");

        let read = registry.get("read_file").expect("tool");
        let read = read
            .execute(ctx(), json!({"path": "src/generated.rs"}))
            .await
            .expect("read");
        assert!(read.content.contains("stress_marker"));

        let grep = registry.get("grep").expect("tool");
        let grep = grep
            .execute(ctx(), json!({"pattern": "stress_marker", "path": "src"}))
            .await
            .expect("grep");
        assert!(grep.content.contains("src/generated.rs:1"));
    }

    #[tokio::test]
    async fn edit_requires_existing_old_string() {
        let registry = stress_tools().expect("registry");
        let edit = registry.get("edit_file").expect("tool");
        let missing = edit
            .execute(
                ctx(),
                json!({
                    "path": "src/main.rs",
                    "old_string": "definitely-not-there",
                    "new_string": "x"
                }),
            )
            .await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn real_tools_stay_inside_ram_root() {
        let dir =
            std::env::temp_dir().join(format!("effibuddy-stress-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = stress_real_tools(&dir).expect("registry");
        let write = registry.get("write_file").expect("tool");
        let output = write
            .execute(
                ctx(),
                json!({"path": "notes.md", "content": "real tools, ram root"}),
            )
            .await
            .expect("write");
        assert!(output.content.contains("notes.md"));
        assert!(dir.join("notes.md").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn virtual_registry_matches_builtin_names() {
        let registry = stress_tools().expect("registry");
        let names = registry.names();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "grep",
            "todo_write",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }
}
