//! `workflow_run`: execute a DAG of agents as one "agent group".
//!
//! The public surface is [`WorkflowTool`]. The JavaScript definition DSL lives
//! in [`dsl`], and the DAG executor in [`exec`]; keeping them apart makes the
//! tool schema readable without the engine in the same file.

use std::time::Instant;

use async_trait::async_trait;
use effibuddy_agent_core::{
    ApprovalPolicy, MAX_AGENT_DEPTH, Tool, ToolContext, ToolError, ToolOutput, WorkflowSpec,
    validate_json,
};
use serde_json::{Value, json};

use dsl::define_from_code;

mod dsl;
mod exec;

pub use dsl::WORKFLOW_PRELUDE;
pub(crate) use exec::*;

const MAX_BATCH: usize = 8;
const MAX_TOTAL_NODE_RUNS: usize = 64;

pub struct WorkflowTool;

impl WorkflowTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WorkflowTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow_run"
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn description(&self) -> &str {
        r#"Run an agent group: a directed acyclic graph of named agents with typed data hand-offs. \
Define it either with `spec` (JSON) or with `code` (JavaScript using the `workflow` builder). Each \
node runs in its own subagent session; an edge means "run me after this node and pass its result". \
Set `output_schema` on a node to require JSON and validate it before downstream nodes receive it. \
Use `input` for one run or `inputs` for a batch; the call returns the group output plus per-node \
metadata that the UI renders as a group on the agent graph.

JavaScript example:
const g = workflow.group({
  name: "research-to-review",
  input: { type: "object", required: ["topic"], properties: { topic: { type: "string" } } },
  output: { type: "object", required: ["verdict"], properties: { verdict: { type: "string" } } }
});
g.agent("research", {
  profile: "explore",
  description: "调研",
  prompt: "Research {{input.topic}}. Return JSON: {facts: string[]}.",
  output: { type: "object", required: ["facts"], properties: { facts: { type: "array", items: { type: "string" } } } }
});
g.agent("review", {
  profile: "reviewer",
  description: "验收",
  prompt: "Review these findings: {{research}}",
  output: { type: "object", required: ["verdict"], properties: { verdict: { type: "string" } } }
});
g.link("research", "review");
return g.spec;

Prompt placeholders: {{input}}, {{input.field}}, {{nodeId}}, {{nodeId.field}}. When a prompt has \
no placeholder the host appends an Input/Upstream section automatically."#
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Group display name. Overrides spec.name."
                },
                "description": {
                    "type": "string",
                    "description": "Group description. Overrides spec.description."
                },
                "spec": {
                    "type": "object",
                    "description": "Workflow spec: {name, description, input_schema, output_schema, nodes, edges, max_parallel}."
                },
                "code": {
                    "type": "string",
                    "description": "JavaScript that builds and returns a workflow spec using the `workflow` builder."
                },
                "input": {
                    "description": "Input value for one run."
                },
                "inputs": {
                    "type": "array",
                    "description": "Batch inputs; each runs the group independently."
                },
                "max_parallel": {
                    "type": "integer",
                    "description": "Maximum nodes running at once (1-8). Overrides spec.max_parallel."
                },
                "context": {
                    "type": "string",
                    "description": "Shared context appended to every node prompt."
                }
            }
        })
    }

    fn summarize(&self, arguments: &Value) -> String {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| {
                arguments
                    .get("spec")
                    .and_then(|spec| spec.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("智能体组");
        let inputs = arguments
            .get("inputs")
            .and_then(Value::as_array)
            .map(|values| values.len())
            .unwrap_or(1);
        format!("运行智能体组「{name}」（{inputs} 组输入，节点由运行时展开）")
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let spawner = ctx.subagents.clone().ok_or_else(|| {
            ToolError::new("Agent groups are not available in this host (no spawner configured).")
        })?;
        if ctx.agent_depth + 1 > MAX_AGENT_DEPTH {
            return Err(ToolError::new(format!(
                "Agent tree depth limit reached ({MAX_AGENT_DEPTH}); this agent cannot start a group."
            )));
        }

        let has_spec = arguments.get("spec").is_some_and(|value| !value.is_null());
        let has_code = arguments
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| !code.trim().is_empty());
        if has_spec == has_code {
            return Err(ToolError::new(
                "workflow_run needs exactly one of `spec` (JSON) or `code` (JavaScript).",
            ));
        }
        let mut spec = if has_code {
            let code = arguments
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            define_from_code(code, ctx.cancel.clone()).await?
        } else {
            serde_json::from_value::<WorkflowSpec>(
                arguments.get("spec").cloned().unwrap_or(Value::Null),
            )
            .map_err(|error| ToolError::new(format!("invalid workflow spec: {error}")))?
        };

        if let Some(name) = arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            spec.name = name.to_string();
        }
        if let Some(description) = arguments
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            spec.description = description.to_string();
        }
        if let Some(parallel) = arguments.get("max_parallel").and_then(Value::as_u64) {
            spec.max_parallel = (parallel as usize).clamp(1, 8);
        }
        spec.max_parallel = spec.max_parallel.clamp(1, 8);
        spec.validate().map_err(ToolError::new)?;

        let inputs = collect_inputs(&arguments)?;
        if inputs.len() > MAX_BATCH {
            return Err(ToolError::new(format!(
                "a batch can contain at most {MAX_BATCH} inputs (got {})",
                inputs.len()
            )));
        }
        if inputs.len() * spec.nodes.len() > MAX_TOTAL_NODE_RUNS {
            return Err(ToolError::new(format!(
                "this run would start {} agents; the limit is {MAX_TOTAL_NODE_RUNS}",
                inputs.len() * spec.nodes.len()
            )));
        }
        for (index, input) in inputs.iter().enumerate() {
            if let Some(schema) = &spec.input_schema {
                validate_json(input, schema).map_err(|error| {
                    ToolError::new(format!("batch input #{}: {error}", index + 1))
                })?;
            }
        }

        let group_id = format!("group_{}", effibuddy_agent_core::ids::short_id());
        let context = arguments
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let started = Instant::now();
        let batch = inputs.len() > 1;
        let mut runs = Vec::new();
        for input in &inputs {
            let run = run_workflow(
                spawner.clone(),
                &ctx,
                &spec,
                input.clone(),
                &group_id,
                &context,
            )
            .await;
            runs.push(run);
            if runs.last().is_some_and(|run| run.status == "failed") {
                break;
            }
        }

        let overall_status = if runs.iter().all(|run| run.status == "completed") {
            "completed"
        } else {
            "failed"
        };
        let overall_error = runs.iter().find_map(|run| run.error.clone());
        let input_json = if batch {
            Value::Array(inputs.clone())
        } else {
            inputs.first().cloned().unwrap_or(Value::Null)
        };
        let output_json = if batch {
            Value::Array(runs.iter().map(|run| run.output.clone()).collect())
        } else {
            runs.first()
                .map(|run| run.output.clone())
                .unwrap_or(Value::Null)
        };
        let nodes: Vec<Value> = runs
            .iter()
            .flat_map(|run| run.nodes.iter().map(NodeExecution::to_json))
            .collect();
        let runs_json: Vec<Value> = runs.iter().map(RunResult::to_json).collect();
        let duration_ms = started.elapsed().as_millis() as u64;

        let workflow = json!({
            "id": group_id,
            "name": spec.name,
            "description": spec.description,
            "status": overall_status,
            "error": overall_error,
            "input": input_json,
            "output": output_json,
            "input_schema": spec.input_schema,
            "output_schema": spec.output_schema,
            "edges": spec.edges,
            "max_parallel": spec.max_parallel,
            "nodes": nodes,
            "runs": runs_json,
            "duration_ms": duration_ms,
        });

        let content = format_text(&spec, &workflow, &runs, overall_status);
        let output = ToolOutput::text(content).with_metadata(json!({"workflow": workflow}));
        Ok(if overall_status == "failed" {
            output.with_error()
        } else {
            output
        })
    }
}

#[cfg(test)]
mod tests;
