//! Programmatic Tool Calling (PTC).
//!
//! Instead of one model tool call per round trip, the model can submit a short
//! JavaScript program that orchestrates many tools locally:
//!
//! ```js
//! const files = tools.list_dir({ path: "crates" });
//! const todos = files
//!   .filter((entry) => entry.kind === "dir")
//!   .map((entry) => tools.grep({ pattern: "TODO", path: entry.path }));
//! return todos;
//! ```
//!
//! The script runs in a Boa (pure Rust) sandbox with no ambient I/O. Every
//! `tools.*` call is proxied back to the harness, which applies each tool's
//! approval policy, timeout and output truncation.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use effibuddy_agent_core::{
    ApprovalPolicy, NestedToolInvoker, Tool, ToolContext, ToolError, ToolOutput,
};
use effibuddy_agent_js::{JsSandbox, SandboxConfig, ToolInvoker};

use crate::args::required_string;

#[derive(Clone)]
pub struct RunJsTool {
    config: SandboxConfig,
}

impl RunJsTool {
    pub fn new() -> Self {
        Self {
            config: SandboxConfig::default(),
        }
    }

    pub fn with_config(config: SandboxConfig) -> Self {
        Self { config }
    }
}

impl Default for RunJsTool {
    fn default() -> Self {
        Self::new()
    }
}

struct NestedJsInvoker {
    nested: Arc<dyn NestedToolInvoker>,
}

#[async_trait]
impl ToolInvoker for NestedJsInvoker {
    async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        self.nested.invoke(name, arguments, cancel).await
    }
}

#[async_trait]
impl Tool for RunJsTool {
    fn name(&self) -> &str {
        "run_js"
    }

    fn description(&self) -> &str {
        "Execute a short JavaScript program that orchestrates tools (programmatic tool calling). \
         Use this instead of many separate tool calls when you need loops, filtering, joins, \
         or to keep large intermediate results out of the conversation.\n\
         \n\
         Contract:\n\
         - The script is synchronous. `await`, `import` and timers are unavailable.\n\
         - Every registered tool is exposed as `tools.<name>(argsObject)`. It returns the tool's \
           observation, parsed as JSON when possible, and throws on tool errors.\n\
         - Return a JSON-serializable value with `return`; it becomes the tool result.\n\
         - `console.log(...)` output is captured and returned.\n\
         - Tool calls are still approval-checked individually.\n\
         - Prefer compact results. Do not dump entire files; select the fields you need.\n\
         \n\
         Example:\n\
         ```js\n\
         const entries = tools.list_dir({ path: \"crates\" });\n\
         const dirs = entries.entries.filter((entry) => entry.kind === \"dir\");\n\
         return dirs.map((entry) => entry.name);\n\
         ```"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Synchronous JavaScript program. Use `return` for the result."
                }
            },
            "required": ["code"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        // The script itself has no ambient capabilities. Every nested tool call
        // goes through its own approval policy in the harness.
        ApprovalPolicy::Auto
    }

    fn summarize(&self, arguments: &Value) -> String {
        let code = arguments
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("(missing code)");
        let first_line = code
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        format!("Run JavaScript: {}", first_line.trim())
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let code = required_string(&arguments, "code")?;
        let nested = ctx.nested.clone().ok_or_else(|| {
            ToolError::new("this runtime does not expose nested tool calls to scripts")
        })?;

        let mut tool_names = nested.tool_names();
        tool_names.retain(|name| name != self.name());
        if tool_names.is_empty() {
            return Err(ToolError::new(
                "no tools are available to the script in this session",
            ));
        }

        let sandbox = JsSandbox::new(self.config.clone());
        let result = sandbox
            .run(
                code,
                tool_names,
                Arc::new(NestedJsInvoker { nested }),
                ctx.cancel.clone(),
            )
            .await
            .map_err(|error| {
                ToolError::new(format!(
                    "JavaScript execution failed: {error}\nFix the script and try again."
                ))
            })?;

        let calls: Vec<Value> = result
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "name": call.name,
                    "ok": call.ok,
                    "duration_ms": call.duration_ms,
                    "preview": preview(&call.content, 240),
                })
            })
            .collect();

        let observation = render_payload(
            result.value,
            result.logs,
            calls,
            result.duration_ms,
            self.config.max_output_chars,
        );
        Ok(ToolOutput::text(observation))
    }
}

/// Serialize the observation as compact JSON within `budget` characters.
///
/// Compact JSON matters: this string goes straight into the transcript, and
/// pretty-printing inflates list-heavy values several-fold. When the payload
/// exceeds the budget, shed auxiliary fields before touching the primary
/// value: `logs` are the bulkiest and most replaceable (rerun the script to
/// reproduce them), then the nested-call previews, and only as a last resort
/// truncate the value itself. This keeps the harness from chopping the JSON
/// apart mid-structure.
fn render_payload(
    value: Value,
    logs: Vec<String>,
    calls: Vec<Value>,
    duration_ms: u64,
    budget: usize,
) -> String {
    fn rendered(object: &serde_json::Map<String, Value>) -> String {
        serde_json::to_string(object).unwrap_or_default()
    }

    let mut object = serde_json::Map::new();
    object.insert("ok".to_string(), json!(true));
    object.insert("value".to_string(), value);
    object.insert(
        "logs".to_string(),
        Value::Array(logs.into_iter().map(Value::String).collect()),
    );
    object.insert("tool_calls".to_string(), Value::Array(calls));
    object.insert("duration_ms".to_string(), json!(duration_ms));

    let mut text = rendered(&object);
    if text.chars().count() <= budget {
        return text;
    }
    object.remove("logs");
    text = rendered(&object);
    if text.chars().count() <= budget {
        return text;
    }
    object.remove("tool_calls");
    text = rendered(&object);
    if text.chars().count() <= budget {
        return text;
    }

    // The value alone is over budget. Truncate it as a string; JSON escaping
    // inflates the embedded text (quotes, newlines), so shave until the
    // rendered envelope actually fits.
    let value_text = serde_json::to_string(&object.get("value").cloned().unwrap_or(Value::Null))
        .unwrap_or_default();
    object.insert("value_truncated".to_string(), json!(true));
    let mut keep = budget.saturating_sub(256);
    loop {
        object.insert(
            "value".to_string(),
            Value::String(format!(
                "{}\n…[value truncated to {keep} characters]",
                truncate_chars(&value_text, keep),
            )),
        );
        let text = rendered(&object);
        let count = text.chars().count();
        if count <= budget {
            return text;
        }
        if keep == 0 {
            // Pathological escaping: give up on the JSON envelope.
            return format!(
                "{{\"ok\":true,\"value_truncated\":true,\"note\":\"value of {} characters is too large to include; rerun the script and return smaller pieces\"}}",
                value_text.chars().count()
            );
        }
        keep = keep.saturating_sub(count - budget + 64);
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn preview(text: &str, max_chars: usize) -> String {
    truncate_chars(text, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    struct HugeEchoInvoker;

    #[async_trait::async_trait]
    impl NestedToolInvoker for HugeEchoInvoker {
        fn tool_names(&self) -> Vec<String> {
            vec!["echo".to_string()]
        }
        async fn invoke(
            &self,
            _name: &str,
            _arguments: Value,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            Ok("x".repeat(60_000))
        }
    }

    fn context_with_huge_echo() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            session_id: "test-session".to_string(),
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            call_id: "call-1".to_string(),
            cancel: CancellationToken::new(),
            nested: Some(Arc::new(HugeEchoInvoker)),
            subagents: None,
        }
    }

    #[tokio::test]
    async fn oversized_observations_shed_logs_previews_then_truncate_value() {
        let tool = RunJsTool::new();
        let budget = SandboxConfig::default().max_output_chars;
        let code = r#"
            console.log("noisy log line ".repeat(200));
            const big = tools.echo({});
            return { big, note: "kept" };
        "#;
        let output = tool
            .execute(context_with_huge_echo(), json!({ "code": code }))
            .await
            .unwrap();

        let text = &output.content;
        assert!(
            text.chars().count() <= budget,
            "observation must respect the budget, got {}",
            text.chars().count()
        );
        assert!(
            !text.contains("noisy log line"),
            "logs must be dropped first"
        );
        assert!(
            !text.contains("tool_calls"),
            "nested-call previews must be dropped next"
        );

        // The envelope stays valid JSON with an explicit truncation marker.
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value_truncated"], true);
        assert_eq!(parsed["logs"], Value::Null);
        assert_eq!(parsed["tool_calls"], Value::Null);
    }

    #[tokio::test]
    async fn compact_payload_keeps_auxiliary_fields_within_budget() {
        let tool = RunJsTool::new();
        let code = r#"
            console.log("small log");
            const value = tools.echo({});
            return value.slice(0, 10);
        "#;
        let output = tool
            .execute(context_with_huge_echo(), json!({ "code": code }))
            .await
            .unwrap();

        // Small observations keep everything — compact, no truncation marker.
        let parsed: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"], "xxxxxxxxxx");
        assert_eq!(parsed["logs"], json!(["small log"]));
        assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 1);
    }
}
