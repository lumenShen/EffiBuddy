//! Shared types inside the agent runtime.
//!
//! Keeping the prepared child, the registry record and the visibility rule in
//! one small module stops the runtime, the registry and the tests from
//! duplicating lifecycle details.

use std::sync::Arc;

use effibuddy_agent_core::{
    AgentDescriptor, AgentNodeKind, AgentVisibility, HarnessConfig, ModelProvider, SharedSession,
    SubagentColor, SubagentProfile, SubagentReport, SubagentRequest, SubagentStatus, ToolRegistry,
    Usage, find_profile, lock_session,
};
use tokio_util::sync::CancellationToken;

/// Everything needed to run a child once (and to wake it again later).
#[derive(Clone)]
pub(crate) struct PreparedChild {
    pub request: SubagentRequest,
    pub profile: SubagentProfile,
    pub name: String,
    pub shared: SharedSession,
    pub tools: Arc<ToolRegistry>,
    pub config: HarnessConfig,
    /// The provider serving this child: the parent's unless the profile
    /// overrides the model.
    pub provider: Arc<dyn ModelProvider>,
    pub allow_spawn: bool,
    pub read_only: bool,
}

/// Registry entry for a live or recently finished child.
#[derive(Clone)]
pub(crate) struct AgentRecord {
    pub descriptor: AgentDescriptor,
    pub prepared: PreparedChild,
    pub cancel: CancellationToken,
    pub status: SubagentStatus,
    /// A message arrived while the run was in flight. If the run ends before
    /// consuming it, start a new turn.
    pub pending_wake: bool,
}

/// Visibility rule: the main agent and group nodes are public; otherwise a
/// private object is addressable only by its creator.
pub(crate) fn can_address(viewer_id: &str, target: &AgentDescriptor) -> bool {
    match target.kind {
        AgentNodeKind::Main | AgentNodeKind::Group => true,
        AgentNodeKind::Agent => match target.visibility {
            AgentVisibility::Public => true,
            AgentVisibility::Private => {
                target.owner_id.as_deref() == Some(viewer_id) || target.id == viewer_id
            }
        },
    }
}

pub(crate) fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "shell")
}

pub(crate) fn descriptor_for(agent_id: &str, prepared: &PreparedChild) -> AgentDescriptor {
    let owner_id = prepared
        .request
        .parent_agent_id
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let owner_name = prepared
        .request
        .parent_agent_name
        .clone()
        .unwrap_or_else(|| owner_id.clone());
    AgentDescriptor {
        id: agent_id.to_string(),
        name: prepared.name.clone(),
        description: prepared.request.description.clone(),
        kind: AgentNodeKind::Agent,
        profile: prepared.profile.name.clone(),
        parent_id: prepared.request.parent_agent_id.clone(),
        parent_name: prepared.request.parent_agent_name.clone(),
        owner_id: Some(owner_id),
        owner_name: Some(owner_name),
        visibility: prepared.request.visibility,
        group_id: prepared.request.group_id.clone(),
        group_name: prepared.request.group_name.clone(),
        depth: prepared.request.depth,
        status: Some(SubagentStatus::Running),
        session_id: Some(lock_session(&prepared.shared).id.clone()),
        can_spawn: prepared.allow_spawn,
    }
}

pub(crate) fn failed_report(
    request: &SubagentRequest,
    agent_id: String,
    child_session_id: String,
    error: String,
) -> SubagentReport {
    let profile = find_profile(&request.profile);
    SubagentReport {
        agent_id,
        name: request
            .agent_name
            .clone()
            .unwrap_or_else(|| request.profile.clone()),
        profile: request.profile.clone(),
        color: profile
            .map(|profile| profile.color)
            .unwrap_or(SubagentColor::Blue),
        description: request.description.clone(),
        child_session_id,
        parent_call_id: request.parent_call_id.clone(),
        parent_agent_id: request.parent_agent_id.clone(),
        group_id: request.group_id.clone(),
        group_name: request.group_name.clone(),
        visibility: request.visibility,
        background: request.background,
        wake: false,
        status: SubagentStatus::Failed,
        report: String::new(),
        data: None,
        error: Some(error),
        tool_uses: 0,
        duration_ms: 0,
        usage: Usage::default(),
    }
}
