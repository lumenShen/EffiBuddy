//! Events flowing from the agent runtime into the GPUI thread.
//!
//! The main harness and every subagent share one foreground pump, so the UI
//! observes a single serialized stream. Child events are tagged with the agent
//! id and never enter the parent transcript.

use std::time::Instant;

use effibuddy_agent_core::{
    AgentMessage, AgentVisibility, HarnessEvent, SharedSession, SubagentColor, SubagentReport,
};

#[derive(Clone)]
#[allow(dead_code)]
pub enum AppEvent {
    Main(HarnessEvent),
    SubagentSpawned(SubagentSpawnedInfo),
    SubagentHarness {
        agent_id: String,
        event: HarnessEvent,
    },
    SubagentFinished(SubagentReport),
    /// A message addressed to the main agent. The app owns the main transcript
    /// and decides whether to wake it, so the runtime only forwards the
    /// envelope.
    AgentMessageToMain(AgentMessage),
    /// A message delivered to a child agent. Used by the graph to draw message
    /// edges and update the inspector.
    AgentMessageDelivered(AgentMessageDeliveredInfo),
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct SubagentSpawnedInfo {
    pub agent_id: String,
    /// Model-facing handle (`researcher`, `reviewer-2`).
    pub name: String,
    pub profile: String,
    pub color: SubagentColor,
    pub description: String,
    pub child_session_id: String,
    pub parent_call_id: String,
    pub parent_session_id: String,
    pub parent_agent_id: Option<String>,
    pub parent_agent_name: Option<String>,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    pub depth: usize,
    /// Private agents are addressable only by their creator; public agents can
    /// be reused by any agent in the session.
    pub visibility: AgentVisibility,
    pub read_only: bool,
    pub background: bool,
    pub can_spawn: bool,
    pub provider_id: Option<String>,
    pub model: String,
    pub started_at: Instant,
    /// Shared state of the child session. The read-only transcript pane reads
    /// this directly while the child is running.
    pub shared: SharedSession,
    /// The full task prompt, shown as the node's input.
    pub task_prompt: String,
    pub output_schema: Option<serde_json::Value>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct AgentMessageDeliveredInfo {
    pub message: AgentMessage,
    pub target_session_id: String,
    pub queued: bool,
    pub woke: bool,
}
