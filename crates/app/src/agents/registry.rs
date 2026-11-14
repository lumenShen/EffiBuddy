//! Address book and lifecycle registry for agent objects.
//!
//! The registry owns the two pieces of mutable state that used to spread
//! through the runtime: the agent records and the normalized name index. All
//! visibility checks and pending-message bookkeeping live here too, so the
//! executor only orchestrates model runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use effibuddy_agent_core::{
    AgentDescriptor, AgentNodeKind, AgentVisibility, Message, SessionState, SubagentProfile,
    SubagentRequest, SubagentStatus, lock_session, normalize_agent_name,
};
use effibuddy_agent_tools::agent::sanitize_agent_name;
use tokio_util::sync::CancellationToken;

use super::types::{AgentRecord, PreparedChild, can_address};

#[derive(Clone)]
pub(crate) struct AgentRegistry {
    main: AgentDescriptor,
    agents: Arc<Mutex<HashMap<String, AgentRecord>>>,
    names: Arc<Mutex<HashMap<String, String>>>,
}

#[allow(dead_code)]
impl AgentRegistry {
    pub fn new(main: AgentDescriptor) -> Self {
        let mut names = HashMap::new();
        names.insert("main".to_string(), "main".to_string());
        Self {
            main,
            agents: Arc::new(Mutex::new(HashMap::new())),
            names: Arc::new(Mutex::new(names)),
        }
    }

    pub fn main(&self) -> AgentDescriptor {
        self.main.clone()
    }

    /// Assign a unique, model-facing name. Duplicates get `-2`, `-3`, … so a
    /// workflow can safely reuse the same node id in a batch.
    pub fn assign_name(
        &self,
        agent_id: &str,
        request: &SubagentRequest,
        profile: &SubagentProfile,
    ) -> String {
        let base = request
            .agent_name
            .as_deref()
            .map(sanitize_agent_name)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                let slug = sanitize_agent_name(&request.description);
                if slug.is_empty() { None } else { Some(slug) }
            })
            .unwrap_or_else(|| {
                let slug = sanitize_agent_name(&profile.name);
                if slug.is_empty() {
                    "agent".to_string()
                } else {
                    slug
                }
            });
        let base = if base == "main" {
            "agent".to_string()
        } else {
            base
        };
        let mut names = self
            .names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !names.contains_key(&base) {
            names.insert(base.clone(), agent_id.to_string());
            return base;
        }
        for index in 2..10_000 {
            let candidate = format!("{base}-{index}");
            if !names.contains_key(&candidate) {
                names.insert(candidate.clone(), agent_id.to_string());
                return candidate;
            }
        }
        let fallback = format!("agent-{}", effibuddy_agent_core::ids::short_id());
        names.insert(fallback.clone(), agent_id.to_string());
        fallback
    }

    pub fn register(
        &self,
        agent_id: &str,
        descriptor: AgentDescriptor,
        prepared: &PreparedChild,
        cancel: CancellationToken,
    ) {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        agents.insert(
            agent_id.to_string(),
            AgentRecord {
                descriptor,
                prepared: prepared.clone(),
                cancel,
                status: SubagentStatus::Running,
                pending_wake: false,
            },
        );
    }

    /// Mark a finished run. Returns true when a message arrived during the run
    /// and a follow-up turn is needed.
    pub fn finish(&self, agent_id: &str, status: SubagentStatus) -> bool {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = agents.get_mut(agent_id) {
            record.status = status;
            record.descriptor.status = Some(status);
            if status.is_terminal() && record.pending_wake {
                record.pending_wake = false;
                return true;
            }
        }
        false
    }

    /// Remember that a message arrived while the target was running.
    pub fn mark_queued(&self, agent_id: &str) {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = agents.get_mut(agent_id)
            && record.status == SubagentStatus::Running
        {
            record.pending_wake = true;
        }
    }

    pub fn status(&self, agent_id: &str) -> Option<SubagentStatus> {
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(agent_id)
            .map(|record| record.status)
    }

    /// Atomically turn a pending wake into a running record and return the
    /// prepared child plus a fresh cancellation token.
    pub fn take_wake(&self, agent_id: &str) -> Option<(PreparedChild, CancellationToken)> {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = agents.get_mut(agent_id)?;
        // A terminal record can be woken directly by a new message; a running
        // one must already have a queued wake from the message path.
        if record.status == SubagentStatus::Running {
            return None;
        }
        record.pending_wake = false;
        record.status = SubagentStatus::Running;
        record.descriptor.status = Some(SubagentStatus::Running);
        let token = CancellationToken::new();
        record.cancel = token.clone();
        Some((record.prepared.clone(), token))
    }

    pub fn clear_pending(&self, agent_id: &str) {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = agents.get_mut(agent_id) {
            record.pending_wake = false;
        }
    }

    /// Append a delivered message to the target transcript. Returns the
    /// session id, a snapshot for persistence, and whether the target was
    /// running (in which case a follow-up wake is already scheduled).
    pub fn append_message(
        &self,
        agent_id: &str,
        message: Message,
    ) -> Option<(String, SessionState, bool)> {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = agents.get_mut(agent_id)?;
        let snapshot = {
            let mut session = lock_session(&record.prepared.shared);
            let session_id = session.id.clone();
            session.push_message(message);
            session.touch();
            (session_id, session.clone())
        };
        let running = record.status == SubagentStatus::Running;
        // A delivered message is a pending turn regardless of whether the
        // target is running. Idle targets are woken immediately by the caller;
        // running targets consume it in this run or right after it finishes.
        record.pending_wake = true;
        Some((snapshot.0, snapshot.1, running))
    }

    pub fn clear_all_pending(&self) {
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for record in agents.values_mut() {
            record.pending_wake = false;
        }
    }

    pub fn parent_read_only(&self, request: &SubagentRequest) -> bool {
        let Some(parent_id) = request.parent_agent_id.as_deref() else {
            return false;
        };
        if parent_id == "main" {
            return false;
        }
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        agents
            .get(parent_id)
            .map(|record| record.prepared.read_only)
            .unwrap_or(false)
    }

    pub fn record_read_only(&self, agent_id: &str) -> Option<bool> {
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(agent_id)
            .map(|record| record.prepared.read_only)
    }

    /// Resolve a model-facing address. The second element is the runtime id;
    /// `None` means the main agent.
    pub fn lookup(&self, address: &str) -> Option<(AgentDescriptor, Option<String>)> {
        let key = normalize_agent_name(address);
        if key.is_empty() {
            return None;
        }
        if matches!(
            key.as_str(),
            "main" | "main-agent" | "parent" | "主智能体" | "主代理"
        ) {
            return Some((self.main.clone(), None));
        }
        let agent_id = {
            let names = self
                .names
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            names.get(&key).cloned()
        }
        .or_else(|| {
            let agents = self
                .agents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            agents
                .values()
                .find(|record| record.descriptor.id == address)
                .map(|record| record.descriptor.id.clone())
        });
        if let Some(agent_id) = agent_id {
            let agents = self
                .agents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return agents
                .get(&agent_id)
                .map(|record| (record.descriptor.clone(), Some(agent_id.clone())));
        }
        // Group nodes are not messageable, but resolving them gives a precise
        // error instead of "unknown agent".
        self.all()
            .into_iter()
            .find(|descriptor| {
                descriptor.kind == AgentNodeKind::Group
                    && (normalize_agent_name(&descriptor.name) == key || descriptor.id == address)
            })
            .map(|descriptor| {
                let id = descriptor.id.clone();
                (descriptor, Some(id))
            })
    }

    pub fn sender_descriptor(
        &self,
        parent_session_id: &str,
        from_session: &str,
    ) -> AgentDescriptor {
        if from_session == parent_session_id {
            return self.main.clone();
        }
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        agents
            .values()
            .find(|record| record.descriptor.session_id.as_deref() == Some(from_session))
            .map(|record| record.descriptor.clone())
            .unwrap_or_else(|| self.main.clone())
    }

    /// Every graph node known to this service: main, agents, and group
    /// pseudo-nodes aggregated from member records.
    pub fn all(&self) -> Vec<AgentDescriptor> {
        let mut out = vec![self.main.clone()];
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut groups: HashMap<String, (String, SubagentStatus)> = HashMap::new();
        for record in agents.values() {
            out.push(record.descriptor.clone());
            if let Some(group_id) = &record.descriptor.group_id {
                let group_name = record
                    .descriptor
                    .group_name
                    .clone()
                    .unwrap_or_else(|| group_id.clone());
                let entry = groups
                    .entry(group_id.clone())
                    .or_insert((group_name, SubagentStatus::Completed));
                if record.status == SubagentStatus::Running {
                    entry.1 = SubagentStatus::Running;
                } else if record.status == SubagentStatus::Failed
                    && entry.1 != SubagentStatus::Running
                {
                    entry.1 = SubagentStatus::Failed;
                }
            }
        }
        for (id, (name, status)) in groups {
            out.push(AgentDescriptor {
                id,
                name,
                description: "智能体组".to_string(),
                kind: AgentNodeKind::Group,
                profile: "group".to_string(),
                parent_id: Some("main".to_string()),
                parent_name: Some("main".to_string()),
                owner_id: Some("main".to_string()),
                owner_name: Some("main".to_string()),
                visibility: AgentVisibility::Public,
                group_id: None,
                group_name: None,
                depth: 1,
                status: Some(status),
                session_id: None,
                can_spawn: true,
            });
        }
        out.sort_by(|left, right| {
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.id.cmp(&right.id))
        });
        out
    }

    pub fn visible(&self, viewer_agent_id: &str) -> Vec<AgentDescriptor> {
        self.all()
            .into_iter()
            .filter(|agent| can_address(viewer_agent_id, agent))
            .collect()
    }

    /// Names currently in use (tests and diagnostics).
    #[cfg(test)]
    pub fn names(&self) -> std::collections::HashSet<String> {
        self.names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}
