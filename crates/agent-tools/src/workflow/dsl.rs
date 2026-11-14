//! JavaScript definition DSL for workflows.
//!
//! The script only builds a serializable spec; it cannot call tools or models.
//! The host executes the returned DAG through the normal subagent runtime.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use effibuddy_agent_core::{ToolError, WorkflowSpec};
use effibuddy_agent_js::{JsSandbox, SandboxConfig, ToolInvoker};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// JavaScript prelude available to `workflow_run.code`. It builds a plain
/// object; no host capability is exposed.
pub const WORKFLOW_PRELUDE: &str = r##"
const workflow = {
  group(options = {}) {
    if (typeof options === "string") options = { name: options };
    const spec = {
      name: String(options.name || options.title || "workflow"),
      description: String(options.description || ""),
      input_schema: options.input || options.inputSchema || null,
      output_schema: options.output || options.outputSchema || null,
      nodes: [],
      edges: [],
      max_parallel: Math.max(1, Math.floor(Number(options.maxParallel || options.max_parallel || 2))),
    };
    const builder = {
      spec: spec,
      agent(id, options = {}) {
        let nodeId = id;
        let node = options;
        if (id && typeof id === "object") { node = id; nodeId = node.id; }
        nodeId = String(nodeId || ("node_" + (spec.nodes.length + 1)));
        const entry = {
          id: nodeId,
          name: String(node.name || node.title || nodeId),
          description: String(node.description || ""),
          profile: String(node.profile || node.type || node.agent || "general-purpose"),
          prompt: String(node.prompt || node.instructions || ""),
          tools: Array.isArray(node.tools) ? node.tools.map(String) : [],
          output_schema: node.output || node.outputSchema || node.schema || null,
          can_spawn: Boolean(node.canSpawn || node.can_spawn),
          read_only: Boolean(node.readOnly || node.read_only),
          visibility: String(node.visibility || "public"),
        };
        spec.nodes.push(entry);
        return nodeId;
      },
      link(from, to) {
        spec.edges.push({ from: String(from), to: String(to) });
        return builder;
      },
      chain() {
        const ids = Array.prototype.slice.call(arguments).map(String);
        for (let i = 0; i + 1 < ids.length; i++) builder.link(ids[i], ids[i + 1]);
        return builder;
      },
      output(id) {
        spec.edges.push({ from: String(id), to: "@output" });
        return builder;
      },
      toJSON() { return spec; },
    };
    return builder;
  },
};
"##;

struct NoTools;

#[async_trait]
impl ToolInvoker for NoTools {
    async fn invoke(
        &self,
        name: &str,
        _arguments: Value,
        _cancel: &CancellationToken,
    ) -> Result<String, String> {
        Err(format!(
            "workflow definitions cannot call tools (`{name}` is not available)"
        ))
    }
}

// ---------------------------------------------------------------------------
// JS definition
// ---------------------------------------------------------------------------

pub(super) async fn define_from_code(
    code: &str,
    cancel: CancellationToken,
) -> Result<WorkflowSpec, ToolError> {
    let sandbox = JsSandbox::new(SandboxConfig {
        timeout: Duration::from_secs(20),
        tool_wait_timeout: Duration::from_secs(5),
        max_code_chars: 48_000,
        max_logs: 50,
        prelude: WORKFLOW_PRELUDE.to_string(),
        ..SandboxConfig::default()
    });
    let result = sandbox
        .run(code, Vec::new(), Arc::new(NoTools), cancel)
        .await
        .map_err(|error| ToolError::new(format!("workflow definition failed: {error}")))?;
    let mut value = result.value;
    if let Value::String(text) = &value {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            value = parsed;
        }
    }
    if value.get("nodes").is_none() {
        if let Some(inner) = value.get("spec") {
            value = inner.clone();
        }
    }
    if value.is_null() {
        let logs = if result.logs.is_empty() {
            String::new()
        } else {
            format!("\nlogs: {}", result.logs.join(" | "))
        };
        return Err(ToolError::new(format!(
            "workflow code returned no spec; finish with `return g.spec;`{logs}"
        )));
    }
    normalize_js_numbers(&mut value);
    serde_json::from_value::<WorkflowSpec>(value).map_err(|error| {
        ToolError::new(format!(
            "workflow code did not return a valid spec: {error}. Finish with `return g.spec;`"
        ))
    })
}

/// JavaScript has one number type, so Boa serializes `2` as `2.0`. Rust
/// integer fields need the integral form; normalize before deserializing.
fn normalize_js_numbers(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_js_numbers(item);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                normalize_js_numbers(item);
            }
        }
        Value::Number(number) => {
            if let Some(integer) = number.as_f64().and_then(|number| {
                if number.is_finite() && number.fract() == 0.0 && number >= 0.0 {
                    Some(number as u64)
                } else {
                    None
                }
            }) {
                *value = Value::Number(serde_json::Number::from(integer));
            }
        }
        _ => {}
    }
}
