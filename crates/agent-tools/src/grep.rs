//! Workspace search tool.

use std::path::Path;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use effibuddy_agent_core::{Tool, ToolContext, ToolError, ToolOutput};

use crate::args::{optional_bool, optional_string, optional_u64, required_string};
use crate::files::is_ignored_dir;
use crate::workspace::Workspace;

const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MATCH_LINES: usize = 500;
const MAX_LINE_CHARS: usize = 400;

#[derive(Clone)]
pub struct GrepTool {
    workspace: Workspace,
}

impl GrepTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search workspace text files with a regular expression. Returns \
         `path:line: text` matches. Skips binaries and common build directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Rust regex syntax."},
                "path": {"type": "string", "description": "Directory or file to search. Defaults to the workspace root."},
                "case_sensitive": {"type": "boolean", "description": "Defaults to true."},
                "fixed_strings": {"type": "boolean", "description": "Treat the pattern as a literal string."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Defaults to 100."}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let pattern = required_string(&arguments, "pattern")?;
        if pattern.is_empty() {
            return Err(ToolError::new("`pattern` must not be empty"));
        }
        let raw_path = optional_string(&arguments, "path")?.unwrap_or_else(|| ".".to_string());
        let case_sensitive = optional_bool(&arguments, "case_sensitive")?.unwrap_or(true);
        let fixed_strings = optional_bool(&arguments, "fixed_strings")?.unwrap_or(false);
        let max_results = optional_u64(&arguments, "max_results")?
            .unwrap_or(100)
            .clamp(1, MAX_MATCH_LINES as u64) as usize;

        let root = self.workspace.resolve_existing(&raw_path)?;
        let expression = if fixed_strings {
            regex::escape(&pattern)
        } else {
            pattern.clone()
        };
        let expression = if case_sensitive {
            expression
        } else {
            format!("(?i){expression}")
        };
        let regex = Regex::new(&expression)
            .map_err(|error| ToolError::new(format!("invalid regex: {error}")))?;

        let mut matches = Vec::new();
        let mut files_scanned = 0usize;
        let mut truncated = false;
        search_path(
            &self.workspace,
            &root,
            &regex,
            &mut matches,
            &mut files_scanned,
            max_results,
            &mut truncated,
            &ctx,
            0,
        )?;

        if matches.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No matches for `{pattern}` under `{}` ({} files scanned).",
                self.workspace.display_path(&root),
                files_scanned
            )));
        }

        let mut output = format!(
            "{} match{} across {} file{} for `{pattern}`\n",
            matches.len(),
            if matches.len() == 1 { "" } else { "es" },
            files_scanned,
            if files_scanned == 1 { "" } else { "s" }
        );
        output.push_str(&matches.join("\n"));
        if truncated {
            output.push_str("\n[results truncated; narrow the search or raise max_results]");
        }
        Ok(ToolOutput::text(output))
    }
}

#[allow(clippy::too_many_arguments)]
fn search_path(
    workspace: &Workspace,
    path: &Path,
    regex: &Regex,
    matches: &mut Vec<String>,
    files_scanned: &mut usize,
    max_results: usize,
    truncated: &mut bool,
    ctx: &ToolContext,
    depth: usize,
) -> Result<(), ToolError> {
    if ctx.cancelled() {
        return Err(ToolError::new("cancelled"));
    }
    if depth > 16 || matches.len() >= max_results {
        *truncated = true;
        return Ok(());
    }

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(()),
    };

    if metadata.is_file() {
        if metadata.len() > MAX_SEARCH_FILE_BYTES {
            return Ok(());
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(()),
        };
        if bytes.contains(&0) {
            return Ok(());
        }
        *files_scanned += 1;
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if matches.len() >= max_results {
                *truncated = true;
                break;
            }
            if regex.is_match(line) {
                let line: String = line.chars().take(MAX_LINE_CHARS).collect();
                matches.push(format!(
                    "{}:{}: {}",
                    workspace.display_path(path),
                    index + 1,
                    line
                ));
            }
        }
        return Ok(());
    }

    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }

    let mut entries: Vec<_> = match std::fs::read_dir(path) {
        Ok(reader) => reader.filter_map(Result::ok).collect(),
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || is_ignored_dir(&name) {
            continue;
        }
        if matches.len() >= max_results {
            *truncated = true;
            break;
        }
        search_path(
            workspace,
            &entry.path(),
            regex,
            matches,
            files_scanned,
            max_results,
            truncated,
            ctx,
            depth + 1,
        )?;
    }
    Ok(())
}
