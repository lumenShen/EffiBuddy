//! Shell command tool.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use effibuddy_agent_core::{ApprovalPolicy, Tool, ToolContext, ToolError, ToolOutput};

use crate::args::{optional_u64, required_string};
use crate::workspace::Workspace;

const MAX_OUTPUT_CHARS: usize = 24_000;

#[derive(Clone)]
pub struct ShellTool {
    workspace: Workspace,
}

impl ShellTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace. Use for builds, tests and git. \
         Output is captured and truncated. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command line to execute."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 300, "description": "Defaults to 60 seconds."}
            },
            "required": ["command"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("(missing command)");
        format!("Run shell command: {command}")
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let command_line = required_string(&arguments, "command")?;
        if command_line.trim().is_empty() {
            return Err(ToolError::new("`command` must not be empty"));
        }
        let timeout_secs = optional_u64(&arguments, "timeout_secs")?
            .unwrap_or(60)
            .clamp(1, 300);
        let timeout = Duration::from_secs(timeout_secs);

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", &command_line]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-lc", &command_line]);
            command
        };

        command
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| ToolError::new(format!("failed to start shell command: {error}")))?;

        let output = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                return Err(ToolError::new("cancelled"));
            }
            result = tokio::time::timeout(timeout, child.wait_with_output()) => {
                match result {
                    Err(_) => {
                        return Err(ToolError::new(format!(
                            "command timed out after {timeout_secs}s"
                        )));
                    }
                    Ok(Err(error)) => {
                        return Err(ToolError::new(format!(
                            "failed to wait for shell command: {error}"
                        )));
                    }
                    Ok(Ok(output)) => output,
                }
            }
        };

        let stdout = truncate(&String::from_utf8_lossy(&output.stdout), MAX_OUTPUT_CHARS);
        let stderr = truncate(&String::from_utf8_lossy(&output.stderr), MAX_OUTPUT_CHARS);
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".to_string());

        let mut result = format!("exit_code: {code}\n");
        if !stdout.trim().is_empty() {
            result.push_str("\n--- stdout ---\n");
            result.push_str(&stdout);
            if !stdout.ends_with('\n') {
                result.push('\n');
            }
        }
        if !stderr.trim().is_empty() {
            result.push_str("\n--- stderr ---\n");
            result.push_str(&stderr);
            if !stderr.ends_with('\n') {
                result.push('\n');
            }
        }
        if stdout.trim().is_empty() && stderr.trim().is_empty() {
            result.push_str("(no output)");
        }
        Ok(ToolOutput::text(result))
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("\n[output truncated]");
    }
    out
}
