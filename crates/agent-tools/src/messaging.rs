//! Inter-agent messaging tools.
//!
//! `agent_send` is the free-form channel: any agent with the tool can deliver
//! prose or a JSON payload to another agent by name or id. `agent_list` exposes
//! the current address book so the model does not have to remember names.

use async_trait::async_trait;
use effibuddy_agent_core::{
    AgentMessageKind, AgentSendOutcome, Tool, ToolContext, ToolError, ToolOutput,
};
use serde_json::{Value, json};

pub struct AgentSendTool;

impl AgentSendTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AgentSendTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AgentSendTool {
    fn name(&self) -> &str {
        "agent_send"
    }

    fn description(&self) -> &str {
        "Send a message or structured payload to another agent by name or id. Use `agent_list` to \
see the address book. If the target is idle it is woken for a new turn; if it is running the \
message is queued and read on its next model step. Messages delivered to the main agent appear in \
the user's conversation as inter-agent notices, not as user input."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target agent name or id. `main` addresses the main agent."
                },
                "message": {
                    "type": "string",
                    "description": "Human-readable message or instruction."
                },
                "data": {
                    "description": "Optional structured payload (any JSON value)."
                },
                "kind": {
                    "type": "string",
                    "enum": ["message", "data", "result"],
                    "description": "Defaults to data when `data` is present, otherwise message."
                }
            },
            "required": ["to"]
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let to = arguments
            .get("to")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::new("`to` is required for agent_send."))?
            .to_string();
        let text = arguments
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let data = arguments
            .get("data")
            .cloned()
            .filter(|value| !value.is_null());
        if text.is_empty() && data.is_none() {
            return Err(ToolError::new(
                "agent_send needs a `message`, `data`, or both.",
            ));
        }
        let kind = arguments
            .get("kind")
            .and_then(Value::as_str)
            .and_then(AgentMessageKind::parse)
            .unwrap_or(if data.is_some() {
                AgentMessageKind::Data
            } else {
                AgentMessageKind::Message
            });
        let spawner = ctx
            .subagents
            .clone()
            .ok_or_else(|| ToolError::new("Agent messaging is not available in this host."))?;
        let outcome = spawner.send_agent_message(&ctx.session_id, &to, kind, text, data)?;
        Ok(
            ToolOutput::text(format_outcome(&outcome)).with_metadata(json!({
                "to": {
                    "id": outcome.to.id,
                    "name": outcome.to.name,
                    "kind": outcome.to.kind,
                    "status": outcome.to.status,
                },
                "delivered": outcome.delivered,
                "queued": outcome.queued,
                "woke": outcome.woke,
            })),
        )
    }
}

fn format_outcome(outcome: &AgentSendOutcome) -> String {
    let delivery = if outcome.woke {
        "the target was idle and has been woken for a new turn"
    } else if outcome.queued {
        "the target is running; the message is queued for its next model step"
    } else {
        "the message was appended to the target transcript"
    };
    format!(
        "Message delivered to `{}` ({}). Delivery: {}.",
        outcome.to.name, outcome.to.id, delivery
    )
}

pub struct AgentListTool;

impl AgentListTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AgentListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &str {
        "agent_list"
    }

    fn description(&self) -> &str {
        "List the agents this caller may address: the main agent, public agents, and private agents \
owned by this agent. Each row includes id, name, visibility, owner, depth, status and group."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, ctx: ToolContext, _arguments: Value) -> Result<ToolOutput, ToolError> {
        let spawner = ctx
            .subagents
            .clone()
            .ok_or_else(|| ToolError::new("Agent messaging is not available in this host."))?;
        let agents = spawner.visible_agents(&ctx.agent_id);
        let rows: Vec<Value> = agents
            .iter()
            .map(|agent| {
                json!({
                    "id": agent.id,
                    "name": agent.name,
                    "kind": agent.kind,
                    "profile": agent.profile,
                    "description": agent.description,
                    "visibility": agent.visibility,
                    "owner_id": agent.owner_id,
                    "owner_name": agent.owner_name,
                    "parent_id": agent.parent_id,
                    "group_id": agent.group_id,
                    "group_name": agent.group_name,
                    "depth": agent.depth,
                    "status": agent.status,
                    "can_spawn": agent.can_spawn,
                })
            })
            .collect();
        let content = serde_json::to_string_pretty(&json!({
            "count": rows.len(),
            "agents": rows,
        }))
        .unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutput::text(content).with_metadata(json!({"count": agents.len()})))
    }
}
