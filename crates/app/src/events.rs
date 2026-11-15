//! Events flowing from the agent runtime into the GPUI thread.
//!
//! The main harness and every subagent share one foreground pump, so the UI
//! observes a single serialized stream. Every run is tagged with its owning
//! conversation: several sessions may run concurrently, and the app routes
//! each event to the active transcript or to a parked background state. Child
//! events also carry the agent id and never enter the parent transcript.

use std::time::Instant;

use effibuddy_agent_core::{
    AgentMessage, AgentVisibility, HarnessEvent, SharedSession, SubagentColor, SubagentReport,
};
use effibuddy_computer::OverlayEvent;

#[derive(Clone)]
#[allow(dead_code)]
pub enum AppEvent {
    /// A main-harness event. Several conversations may run at the same time,
    /// so the owning session id rides along: the UI routes it to the active
    /// transcript or to that session's parked background state.
    Main {
        session_id: String,
        event: HarnessEvent,
    },
    SubagentSpawned {
        /// Main conversation whose `SubagentService` launched the child. The
        /// info's own `parent_session_id` is the immediate parent session,
        /// which can be another child for nested agents.
        parent_session_id: String,
        info: SubagentSpawnedInfo,
    },
    SubagentHarness {
        /// Main conversation whose `SubagentService` owns the child. Child ids
        /// alone are not enough once two sessions run concurrently.
        parent_session_id: String,
        agent_id: String,
        event: HarnessEvent,
    },
    SubagentFinished {
        parent_session_id: String,
        report: SubagentReport,
    },
    /// A message addressed to the main agent. The app owns the main transcript
    /// and decides whether to wake it, so the runtime only forwards the
    /// envelope.
    AgentMessageToMain {
        parent_session_id: String,
        message: AgentMessage,
    },
    /// A message delivered to a child agent. Used by the graph to draw message
    /// edges and update the inspector.
    AgentMessageDelivered {
        parent_session_id: String,
        info: AgentMessageDeliveredInfo,
    },
    /// Floating computer-use control bar state. Sent from the tool runtime on
    /// the same pump as harness events so consent/overlay updates stay ordered
    /// with tool results.
    ComputerOverlay(OverlayEvent),
    /// Update check finished. `Ok(None)` = the repository has no published
    /// release yet; `Err` = network/rate-limit failure (surfaced only in the
    /// About page, never as a banner).
    UpdateChecked(Result<Option<crate::update::UpdateInfo>, String>),
    /// Stress lab (feature `stress`): a sampled sessions-mode harness event
    /// routed through the pump so the dashboard cards can render it.
    #[cfg(feature = "stress")]
    StressDelta {
        index: usize,
        event: HarnessEvent,
    },
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
