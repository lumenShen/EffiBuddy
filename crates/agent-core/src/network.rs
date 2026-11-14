//! Agent network: identities, inter-agent messages, agent-group workflows and
//! the small data-contract layer used for typed hand-offs.
//!
//! The runtime only depends on these plain data types. The desktop app turns
//! them into events and pixels, while tools use them to describe what a child
//! agent is called, who may talk to whom, and what shape a node must return.
//!
//! Three ideas are represented here:
//!
//! - [`AgentDescriptor`] — an addressable agent (main agent, subagent, or
//!   group). Names are stable handles the model can use with `agent_send`.
//! - [`AgentMessage`] — a message between two agents. Free text and structured
//!   data share one envelope so the UI can render both in the same stream.
//! - [`crate::workflow_spec::WorkflowSpec`] — a small DAG of agents (an "agent
//!   group") with optional JSON Schemas on its input and output.
//!   `workflow_run` executes it; the UI renders it as a group node on the graph
//!   canvas.
//!
//! [`crate::contract::validate_json`] intentionally implements a practical
//! JSON Schema subset (type/required/properties/items/enum/const/combinators
//! and numeric and string bounds). It is a contract check, not a
//! general-purpose validator.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::new_id;
use crate::message::truncate_chars;
use crate::subagent::SubagentStatus;

/// What kind of node this is on the agent canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentNodeKind {
    Main,
    Agent,
    Group,
}

impl AgentNodeKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentNodeKind::Main => "主智能体",
            AgentNodeKind::Agent => "智能体",
            AgentNodeKind::Group => "智能体组",
        }
    }
}

/// Who may address an agent object.
///
/// - `Private` — only the agent that created it (its owner) may list, message,
///   or call it. This is the default for one-off work.
/// - `Public` — any agent in the same session may reuse it. Use this for
///   reusable specialists such as a shared reviewer or a research scout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentVisibility {
    #[default]
    Private,
    Public,
}

impl AgentVisibility {
    pub fn label(self) -> &'static str {
        match self {
            AgentVisibility::Private => "私有",
            AgentVisibility::Public => "公开",
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            AgentVisibility::Private => "private",
            AgentVisibility::Public => "public",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "private" | "owner" | "私有" => Some(AgentVisibility::Private),
            "public" | "shared" | "公开" | "共享" => Some(AgentVisibility::Public),
            _ => None,
        }
    }
}

/// Addressable identity of one agent. `name` is the model-facing handle
/// (`researcher`, `reviewer-2`); `id` is the runtime identity (`agent_…`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentDescriptor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: AgentNodeKind,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub parent_name: Option<String>,
    /// The agent that created this object. A private object is accessible only
    /// to this identity.
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub owner_name: Option<String>,
    #[serde(default)]
    pub visibility: AgentVisibility,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub depth: usize,
    #[serde(default)]
    pub status: Option<SubagentStatus>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub can_spawn: bool,
}

impl AgentDescriptor {
    pub fn main(session_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: "main".to_string(),
            name: name.into(),
            description: "主智能体：与用户对话并调度子代理与智能体组".to_string(),
            kind: AgentNodeKind::Main,
            profile: "main".to_string(),
            parent_id: None,
            parent_name: None,
            owner_id: None,
            owner_name: None,
            visibility: AgentVisibility::Public,
            group_id: None,
            group_name: None,
            depth: 0,
            status: None,
            session_id: Some(session_id.into()),
            can_spawn: true,
        }
    }

    pub fn normalized_name(&self) -> String {
        normalize_agent_name(&self.name)
    }
}

/// Tolerant lookup key: `Researcher 2`, `researcher_2` and `researcher-2`
/// resolve to the same agent.
pub fn normalize_agent_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace([' ', '_'], "-")
        .trim_matches('-')
        .to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// Plain prose between agents.
    Message,
    /// A structured payload (also carries a human-readable summary).
    Data,
    /// A final result hand-back or acknowledgement.
    Result,
}

impl AgentMessageKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentMessageKind::Message => "消息",
            AgentMessageKind::Data => "数据",
            AgentMessageKind::Result => "结果",
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            AgentMessageKind::Message => "message",
            AgentMessageKind::Data => "data",
            AgentMessageKind::Result => "result",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "message" | "text" | "chat" => Some(AgentMessageKind::Message),
            "data" | "json" | "structured" => Some(AgentMessageKind::Data),
            "result" | "back" | "handback" => Some(AgentMessageKind::Result),
            _ => None,
        }
    }
}

/// One message between two agents. The metadata fields are intentionally
/// denormalized: a transcript must stay readable after the sender is gone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMessage {
    #[serde(default = "new_id")]
    pub id: String,
    pub from_id: String,
    pub from_name: String,
    pub to_id: String,
    pub to_name: String,
    #[serde(default = "default_message_kind")]
    pub kind: AgentMessageKind,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_message_kind() -> AgentMessageKind {
    AgentMessageKind::Message
}

impl AgentMessage {
    pub fn new(
        from: &AgentDescriptor,
        to: &AgentDescriptor,
        kind: AgentMessageKind,
        text: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            id: new_id(),
            from_id: from.id.clone(),
            from_name: from.name.clone(),
            to_id: to.id.clone(),
            to_name: to.name.clone(),
            kind,
            text: text.into(),
            data,
            created_at: Utc::now(),
        }
    }

    /// The model-visible text injected into the receiver's transcript.
    pub fn framed_text(&self) -> String {
        let mut out = format!(
            "[agent-message]\nfrom: {} ({})\nto: {} ({})\nkind: {}\n",
            self.from_name,
            self.from_id,
            self.to_name,
            self.to_id,
            self.kind.wire()
        );
        if !self.text.trim().is_empty() {
            out.push_str("\n");
            out.push_str(self.text.trim());
            out.push_str("\n");
        }
        if let Some(data) = &self.data {
            out.push_str("\n[data]\n");
            out.push_str(&serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string()));
            out.push_str("\n");
        }
        out.push_str("\n[/agent-message]");
        out
    }

    pub fn preview(&self, max_chars: usize) -> String {
        let raw = if self.text.trim().is_empty() {
            self.data
                .as_ref()
                .map(|data| serde_json::to_string(data).unwrap_or_default())
                .unwrap_or_default()
        } else {
            self.text.clone()
        };
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_chars(&collapsed, max_chars)
    }
}

/// Result of a delivery attempt. `queued` means the target is busy and will
/// read the message on its next model step; `woke` means it was idle and a new
/// turn was started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSendOutcome {
    pub to: AgentDescriptor,
    pub delivered: bool,
    pub queued: bool,
    pub woke: bool,
}

// ---------------------------------------------------------------------------
// Persisted graph state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphPoint {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
}

pub fn default_graph_zoom() -> f32 {
    1.0
}

/// One graph node as persisted with the session. Live UI tasks overlay this
/// record; after a restart it is all the graph has.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentGraphNodeRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: AgentNodeKind,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub parent_name: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub owner_name: Option<String>,
    #[serde(default)]
    pub visibility: AgentVisibility,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub depth: usize,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tool_uses: usize,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

impl AgentGraphNodeRecord {
    pub fn from_descriptor(descriptor: &AgentDescriptor) -> Self {
        Self {
            id: descriptor.id.clone(),
            name: descriptor.name.clone(),
            description: descriptor.description.clone(),
            kind: descriptor.kind,
            profile: descriptor.profile.clone(),
            parent_id: descriptor.parent_id.clone(),
            parent_name: descriptor.parent_name.clone(),
            owner_id: descriptor.owner_id.clone(),
            owner_name: descriptor.owner_name.clone(),
            visibility: descriptor.visibility,
            group_id: descriptor.group_id.clone(),
            group_name: descriptor.group_name.clone(),
            depth: descriptor.depth,
            status: descriptor
                .status
                .map(|status| status.label().to_string())
                .unwrap_or_else(|| "运行中".to_string()),
            session_id: descriptor.session_id.clone(),
            tool_uses: 0,
            total_tokens: 0,
            task: None,
            output: None,
            data: None,
            input: None,
            input_schema: None,
            output_schema: None,
            started_at: Some(Utc::now()),
            finished_at: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentGraphEdgeRecord {
    pub from: String,
    pub to: String,
    #[serde(default = "default_edge_kind")]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_edge_count")]
    pub count: u32,
}

fn default_edge_kind() -> String {
    "spawn".to_string()
}

fn default_edge_count() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentGraphRecord {
    #[serde(default)]
    pub nodes: Vec<AgentGraphNodeRecord>,
    #[serde(default)]
    pub edges: Vec<AgentGraphEdgeRecord>,
    #[serde(default)]
    pub messages: Vec<AgentMessage>,
    #[serde(default)]
    pub pan: GraphPoint,
    #[serde(default = "default_graph_zoom")]
    pub zoom: f32,
}

impl Default for AgentGraphRecord {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            messages: Vec::new(),
            pan: GraphPoint::default(),
            zoom: 1.0,
        }
    }
}

impl AgentGraphRecord {
    pub fn node(&self, id: &str) -> Option<&AgentGraphNodeRecord> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut AgentGraphNodeRecord> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }

    pub fn upsert_node(&mut self, record: AgentGraphNodeRecord) {
        match self.node_mut(&record.id) {
            Some(existing) => {
                let started_at = existing.started_at.or(record.started_at);
                *existing = record;
                existing.started_at = started_at;
            }
            None => self.nodes.push(record),
        }
    }

    pub fn upsert_edge(&mut self, from: &str, to: &str, kind: &str, label: &str) {
        if let Some(edge) = self
            .edges
            .iter_mut()
            .find(|edge| edge.from == from && edge.to == to && edge.kind == kind)
        {
            edge.count = edge.count.saturating_add(1);
            if !label.trim().is_empty() {
                edge.label = label.to_string();
            }
            return;
        }
        self.edges.push(AgentGraphEdgeRecord {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
            label: label.to_string(),
            count: 1,
        });
    }

    pub fn push_message(&mut self, message: AgentMessage) {
        self.messages.push(message);
        if self.messages.len() > 200 {
            let overflow = self.messages.len() - 200;
            self.messages.drain(0..overflow);
        }
    }

    /// Remove edges that point at nodes which no longer exist. Called after
    /// loading old sessions; it keeps the canvas drawable.
    pub fn prune_edges(&mut self) {
        let ids: HashSet<String> = self
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .chain(std::iter::once("main".to_string()))
            .collect();
        self.edges
            .retain(|edge| ids.contains(&edge.from) && ids.contains(&edge.to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{extract_json_value, validate_json};
    use crate::workflow_spec::{WorkflowEdge, WorkflowNodeSpec, WorkflowSpec, render_template};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn descriptor(id: &str, name: &str) -> AgentDescriptor {
        AgentDescriptor {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            kind: AgentNodeKind::Agent,
            profile: "general-purpose".to_string(),
            parent_id: Some("main".to_string()),
            parent_name: Some("main".to_string()),
            owner_id: None,
            owner_name: None,
            visibility: AgentVisibility::default(),
            group_id: None,
            group_name: None,
            depth: 1,
            status: Some(SubagentStatus::Running),
            session_id: Some("sess".to_string()),
            can_spawn: true,
        }
    }

    #[test]
    fn schema_validates_nested_contracts() {
        let schema = json!({
            "type": "object",
            "required": ["topic", "findings"],
            "additionalProperties": false,
            "properties": {
                "topic": {"type": "string", "minLength": 2},
                "findings": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "required": ["title", "severity"],
                        "properties": {
                            "title": {"type": "string"},
                            "severity": {"enum": ["low", "medium", "high"]}
                        }
                    }
                }
            }
        });
        validate_json(
            &json!({"topic": "graph", "findings": [{"title": "a", "severity": "high"}]}),
            &schema,
        )
        .unwrap();
        let error = validate_json(
            &json!({"topic": "graph", "findings": [{"title": "a", "severity": "urgent"}]}),
            &schema,
        )
        .unwrap_err();
        assert!(error.contains("enum"), "{error}");
        let error = validate_json(&json!({"topic": "g"}), &schema).unwrap_err();
        assert!(error.contains("findings"), "{error}");
    }

    #[test]
    fn extract_json_handles_fences_and_prose() {
        assert_eq!(
            extract_json_value("```json\n{\"a\": 1}\n```"),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            extract_json_value("Here is the result: {\"a\": [1, 2]} — done."),
            Some(json!({"a": [1, 2]}))
        );
        assert_eq!(extract_json_value("no json here"), None);
    }

    #[test]
    fn template_renders_input_and_upstream_values() {
        let mut outputs = BTreeMap::new();
        outputs.insert("research".to_string(), json!({"summary": "ok"}));
        let (rendered, replaced) = render_template(
            "Topic {{input.topic}} / {{research.summary}} / {{missing}}",
            &json!({"topic": "agents"}),
            &outputs,
        );
        assert!(replaced);
        assert!(
            rendered.contains("Topic agents / ok / {{missing}}"),
            "{rendered}"
        );
    }

    #[test]
    fn workflow_rejects_cycles_and_accepts_parallel_layers() {
        let mut spec = WorkflowSpec {
            name: "test".to_string(),
            description: String::new(),
            input_schema: None,
            output_schema: None,
            nodes: vec![node("a"), node("b"), node("c")],
            edges: vec![
                WorkflowEdge {
                    from: "a".to_string(),
                    to: "c".to_string(),
                },
                WorkflowEdge {
                    from: "b".to_string(),
                    to: "c".to_string(),
                },
            ],
            max_parallel: 2,
        };
        spec.validate().unwrap();
        let layers = spec.topological_layers().unwrap();
        assert_eq!(layers, vec![vec!["a", "b"], vec!["c"]]);
        spec.edges.push(WorkflowEdge {
            from: "c".to_string(),
            to: "a".to_string(),
        });
        assert!(spec.validate().unwrap_err().contains("cycle"));
    }

    fn node(id: &str) -> WorkflowNodeSpec {
        WorkflowNodeSpec {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            profile: "general-purpose".to_string(),
            prompt: String::new(),
            tools: Vec::new(),
            output_schema: None,
            can_spawn: false,
            read_only: false,
            visibility: AgentVisibility::Private,
        }
    }

    #[test]
    fn message_frame_is_model_visible_and_self_describing() {
        let from = descriptor("agent_a", "researcher");
        let to = descriptor("agent_b", "writer");
        let message = AgentMessage::new(
            &from,
            &to,
            AgentMessageKind::Data,
            "findings attached",
            Some(json!({"ok": true})),
        );
        let framed = message.framed_text();
        assert!(framed.contains("from: researcher (agent_a)"));
        assert!(framed.contains("[data]"));
        assert!(framed.contains("\"ok\": true"));
    }

    #[test]
    fn graph_edges_merge_and_nodes_upsert() {
        let mut graph = AgentGraphRecord::default();
        graph.upsert_node(AgentGraphNodeRecord::from_descriptor(&descriptor(
            "agent_a",
            "researcher",
        )));
        graph.upsert_edge("main", "agent_a", "spawn", "delegate");
        graph.upsert_edge("main", "agent_a", "spawn", "delegate");
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].count, 2);
        let mut duplicate =
            AgentGraphNodeRecord::from_descriptor(&descriptor("agent_a", "researcher-2"));
        duplicate.status = "已完成".to_string();
        graph.upsert_node(duplicate);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].name, "researcher-2");
    }
}
