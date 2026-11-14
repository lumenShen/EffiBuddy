//! File and directory tools.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use effibuddy_agent_core::{ApprovalPolicy, Tool, ToolContext, ToolError, ToolOutput};

use crate::args::{optional_bool, optional_u64, required_string};
use crate::workspace::Workspace;

const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
const MAX_READ_OUTPUT_CHARS: usize = 60_000;
const MAX_WRITE_CHARS: usize = 1_000_000;
const MAX_LIST_ENTRIES: usize = 500;

#[derive(Clone)]
pub struct ReadFileTool {
    workspace: Workspace,
}

impl ReadFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file from the workspace. Returns numbered lines. \
         Use `offset` (1-based line) and `limit` for large files."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to read (1-based)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum number of lines. Defaults to 400."}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let path = self.workspace.resolve_existing(&raw_path)?;

        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot stat `{raw_path}`: {error}")))?;
        if metadata.is_dir() {
            return Err(ToolError::new(format!(
                "`{raw_path}` is a directory; use list_dir instead"
            )));
        }
        if metadata.len() > MAX_READ_BYTES {
            return Err(ToolError::new(format!(
                "`{raw_path}` is {} bytes; read a smaller file or use offset/limit",
                metadata.len()
            )));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot read `{raw_path}`: {error}")))?;
        if bytes.contains(&0) {
            return Err(ToolError::new(format!(
                "`{raw_path}` looks like a binary file"
            )));
        }
        let text = String::from_utf8_lossy(&bytes);

        let offset = optional_u64(&arguments, "offset")?.unwrap_or(1).max(1) as usize;
        let limit = optional_u64(&arguments, "limit")?
            .unwrap_or(400)
            .clamp(1, 2000) as usize;

        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();
        if offset > total_lines && total_lines > 0 {
            return Err(ToolError::new(format!(
                "offset {offset} is past the end of `{raw_path}` ({total_lines} lines)"
            )));
        }

        let start = (offset - 1).min(total_lines);
        let end = (start + limit).min(total_lines);
        let mut output = format!(
            "{} ({} lines{})\n",
            self.workspace.display_path(&path),
            total_lines,
            if metadata.len() > 0 {
                format!(", {} bytes", metadata.len())
            } else {
                String::new()
            }
        );
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>5} | {}\n", start + index + 1, line));
        }
        if end < total_lines {
            output.push_str(&format!(
                "\n[{} more lines; continue with offset={}]",
                total_lines - end,
                end + 1
            ));
        }
        if output.chars().count() > MAX_READ_OUTPUT_CHARS {
            output = output.chars().take(MAX_READ_OUTPUT_CHARS).collect();
            output.push_str("\n[output truncated]");
        }
        if ctx.cancelled() {
            return Err(ToolError::new("cancelled"));
        }
        Ok(ToolOutput::text(output))
    }
}

#[derive(Clone)]
pub struct WriteFileTool {
    workspace: Workspace,
}

impl WriteFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a UTF-8 text file. Prefer `edit_file` for small \
         changes to existing files. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root."},
                "content": {"type": "string", "description": "Full file contents."}
            },
            "required": ["path", "content"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("(missing path)");
        let bytes = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        format!("Write {bytes} bytes to `{path}`")
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let content = required_string(&arguments, "content")?;
        if content.len() > MAX_WRITE_CHARS {
            return Err(ToolError::new(format!(
                "content is too large ({} characters, limit {MAX_WRITE_CHARS})",
                content.len()
            )));
        }
        let path = self.workspace.resolve_for_write(&raw_path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::new(format!(
                    "cannot create parent directory `{}`: {error}",
                    parent.display()
                ))
            })?;
        }
        tokio::fs::write(&path, content.as_bytes())
            .await
            .map_err(|error| ToolError::new(format!("cannot write `{raw_path}`: {error}")))?;
        Ok(ToolOutput::text(format!(
            "Wrote {} bytes to `{}`.",
            content.len(),
            self.workspace.display_path(&path)
        )))
    }
}

#[derive(Clone)]
pub struct EditFileTool {
    workspace: Workspace,
}

impl EditFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. Fails if `old_string` is missing \
         or appears more than once unless `replace_all` is true. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace, including whitespace."},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence. Defaults to false."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("(missing path)");
        format!("Edit `{path}` in place")
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let old_string = required_string(&arguments, "old_string")?;
        let new_string = required_string(&arguments, "new_string")?;
        let replace_all = optional_bool(&arguments, "replace_all")?.unwrap_or(false);
        if old_string.is_empty() {
            return Err(ToolError::new("`old_string` must not be empty"));
        }

        let path = self.workspace.resolve_existing(&raw_path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot read `{raw_path}`: {error}")))?;

        let occurrences = content.matches(&old_string).count();
        if occurrences == 0 {
            return Err(ToolError::new(format!(
                "`old_string` was not found in `{raw_path}`. Read the file and copy the exact text."
            )));
        }
        if occurrences > 1 && !replace_all {
            return Err(ToolError::new(format!(
                "`old_string` appears {occurrences} times in `{raw_path}`; add surrounding context or set `replace_all`"
            )));
        }

        let updated = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };
        tokio::fs::write(&path, updated.as_bytes())
            .await
            .map_err(|error| ToolError::new(format!("cannot write `{raw_path}`: {error}")))?;

        Ok(ToolOutput::text(format!(
            "Replaced {} occurrence{} in `{}`.",
            if replace_all { occurrences } else { 1 },
            if replace_all && occurrences != 1 {
                "s"
            } else {
                ""
            },
            self.workspace.display_path(&path)
        )))
    }
}

#[derive(Clone)]
pub struct ListDirTool {
    workspace: Workspace,
}

impl ListDirTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and directories under a workspace path as JSON. \
         Skips common build/VCS directories and hidden entries by default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path. Defaults to the workspace root."},
                "depth": {"type": "integer", "minimum": 1, "maximum": 3, "description": "Recursion depth. Defaults to 1."},
                "include_hidden": {"type": "boolean", "description": "Include dotfiles and ignored directories."}
            }
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path =
            crate::args::optional_string(&arguments, "path")?.unwrap_or_else(|| ".".to_string());
        let root = self.workspace.resolve_existing(&raw_path)?;
        if !root.is_dir() {
            return Err(ToolError::new(format!("`{raw_path}` is not a directory")));
        }
        let depth = optional_u64(&arguments, "depth")?.unwrap_or(1).clamp(1, 3) as usize;
        let include_hidden = optional_bool(&arguments, "include_hidden")?.unwrap_or(false);

        let mut entries = Vec::new();
        let mut truncated = false;
        collect_entries(
            &self.workspace,
            &root,
            depth,
            include_hidden,
            &mut entries,
            &mut truncated,
            ctx.cancel.clone(),
        )?;

        let payload = json!({
            "path": self.workspace.display_path(&root),
            "entries": entries,
            "truncated": truncated,
        });
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        ))
    }
}

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
];

pub(crate) fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

fn collect_entries(
    workspace: &Workspace,
    directory: &Path,
    depth: usize,
    include_hidden: bool,
    entries: &mut Vec<Value>,
    truncated: &mut bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::new("cancelled"));
    }
    if *truncated {
        return Ok(());
    }
    let reader = std::fs::read_dir(directory).map_err(|error| {
        ToolError::new(format!(
            "cannot read directory `{}`: {error}",
            directory.display()
        ))
    })?;

    let mut items: Vec<_> = reader.filter_map(Result::ok).collect();
    items.sort_by_key(|entry| {
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        (std::cmp::Reverse(is_dir), entry.file_name())
    });

    for entry in items {
        if entries.len() >= MAX_LIST_ENTRIES {
            *truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let symlink_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let is_symlink = symlink_metadata.file_type().is_symlink();
        let is_dir = symlink_metadata.is_dir() && !is_symlink;
        if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let size = if symlink_metadata.is_file() {
            Some(symlink_metadata.len())
        } else {
            None
        };
        entries.push(json!({
            "name": name,
            "path": workspace.display_path(&path),
            "kind": if is_symlink { "symlink" } else if is_dir { "dir" } else { "file" },
            "size": size,
        }));
        if is_dir && depth > 1 {
            collect_entries(
                workspace,
                &path,
                depth - 1,
                include_hidden,
                entries,
                truncated,
                cancel.clone(),
            )?;
        }
    }
    Ok(())
}
