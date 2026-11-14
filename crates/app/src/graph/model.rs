//! Graph projection: turn persisted records plus live tasks into drawable
//! nodes, group frames and edges.
//!
//! The model is pure data. It contains no GPUI types, which keeps layout
//! testable and lets the canvas decide how to paint.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use effibuddy_agent_core::{
    AgentGraphNodeRecord, AgentNodeKind, AgentVisibility, SessionState, SubagentColor,
    SubagentStatus,
};
use serde_json::Value;

use crate::app::AgentTask;

pub const NODE_W: f32 = 236.0;
pub const NODE_H: f32 = 106.0;
pub const COL_GAP: f32 = 104.0;
pub const ROW_GAP: f32 = 34.0;
pub const FRAME_PAD: f32 = 20.0;
pub const FRAME_HEADER: f32 = 52.0;
pub const FRAME_MEMBER_GAP: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphStatus {
    Idle,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

impl GraphStatus {
    pub fn from_subagent(status: SubagentStatus) -> Self {
        match status {
            SubagentStatus::Running => GraphStatus::Running,
            SubagentStatus::Completed => GraphStatus::Completed,
            SubagentStatus::Failed => GraphStatus::Failed,
            // The graph only shows live or locally finished agents; a restored
            // record with no terminal evidence degrades to failed.
            SubagentStatus::Cancelled | SubagentStatus::Lost => GraphStatus::Failed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GraphStatus::Idle => "空闲",
            GraphStatus::Running => "运行中",
            GraphStatus::WaitingApproval => "等待批准",
            GraphStatus::Completed => "已完成",
            GraphStatus::Failed => "失败",
            GraphStatus::Cancelled => "已停止",
        }
    }

    pub fn parse(label: &str) -> Self {
        match label.trim() {
            "运行中" | "running" | "Running" => GraphStatus::Running,
            "等待批准" | "waiting" => GraphStatus::WaitingApproval,
            "已完成" | "completed" | "Completed" => GraphStatus::Completed,
            "失败" | "failed" | "Failed" => GraphStatus::Failed,
            "已停止" | "cancelled" | "Cancelled" => GraphStatus::Cancelled,
            _ => GraphStatus::Idle,
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, GraphStatus::Running | GraphStatus::WaitingApproval)
    }
}

#[derive(Clone, Debug)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: AgentNodeKind,
    pub profile: String,
    pub status: GraphStatus,
    pub color: SubagentColor,
    pub visibility: AgentVisibility,
    pub owner_id: Option<String>,
    pub owner_name: Option<String>,
    pub parent_id: Option<String>,
    pub group_id: Option<String>,
    pub depth: usize,
    pub can_spawn: bool,
    pub read_only: bool,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub tool_uses: usize,
    pub total_tokens: u64,
    pub duration_ms: u64,
    pub waiting_approval: bool,
    pub task: Option<String>,
    pub output: Option<String>,
    pub data: Option<Value>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub live: bool,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl GraphNode {
    pub fn is_main(&self) -> bool {
        self.kind == AgentNodeKind::Main
    }

    pub fn rect(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.w, self.h)
    }
}

#[derive(Clone, Debug)]
pub struct GraphFrame {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: GraphStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub data: Option<Value>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub error: Option<String>,
    pub member_ids: Vec<String>,
    pub live: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl GraphFrame {
    pub fn rect(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.w, self.h)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphEdgeKind {
    Spawn,
    Message,
    Data,
}

impl GraphEdgeKind {
    pub fn label(self) -> &'static str {
        match self {
            GraphEdgeKind::Spawn => "召唤",
            GraphEdgeKind::Message => "消息",
            GraphEdgeKind::Data => "数据",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "message" => GraphEdgeKind::Message,
            "data" => GraphEdgeKind::Data,
            _ => GraphEdgeKind::Spawn,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: GraphEdgeKind,
    pub label: String,
    pub count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct GraphProjection {
    pub nodes: Vec<GraphNode>,
    pub frames: Vec<GraphFrame>,
    pub edges: Vec<GraphEdge>,
    pub bounds: (f32, f32),
    /// id → index into `nodes` / `frames`, built once so per-frame edge and
    /// inspector lookups stay O(1) instead of scanning every node.
    node_index: HashMap<String, usize>,
    frame_index: HashMap<String, usize>,
}

impl GraphProjection {
    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.node_index
            .get(id)
            .and_then(|index| self.nodes.get(*index))
    }

    pub fn frame(&self, id: &str) -> Option<&GraphFrame> {
        self.frame_index
            .get(id)
            .and_then(|index| self.frames.get(*index))
    }

    pub fn rect(&self, id: &str) -> Option<(f32, f32, f32, f32)> {
        self.node(id)
            .map(GraphNode::rect)
            .or_else(|| self.frame(id).map(GraphFrame::rect))
    }
}

/// Build the drawable graph for the active session. Live tasks overlay the
/// persisted record; group frames aggregate their member nodes.
pub fn build_projection(
    session: &SessionState,
    tasks: &[AgentTask],
    main_running: bool,
) -> GraphProjection {
    let record = &session.graph;
    let tasks_by_agent: HashMap<&str, &AgentTask> = tasks
        .iter()
        .map(|task| (task.agent_id.as_str(), task))
        .collect();

    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    for stored in &record.nodes {
        if stored.kind == AgentNodeKind::Group {
            continue;
        }
        nodes.insert(
            stored.id.clone(),
            node_from_record(stored, tasks_by_agent.get(stored.id.as_str()).copied()),
        );
    }
    // Live tasks without a persisted record (for example during preview or a
    // race between spawn and persistence) still appear immediately.
    for task in tasks {
        nodes
            .entry(task.agent_id.clone())
            .and_modify(|node| overlay_task(node, task))
            .or_insert_with(|| node_from_task(task));
    }

    let main = main_node(session, main_running);
    nodes.insert(main.id.clone(), main);

    // Group frames: persisted group nodes + any group referenced by a task.
    let mut groups: BTreeMap<String, GraphFrame> = BTreeMap::new();
    for stored in &record.nodes {
        if stored.kind != AgentNodeKind::Group {
            continue;
        }
        groups.insert(
            stored.id.clone(),
            GraphFrame {
                id: stored.id.clone(),
                name: stored.name.clone(),
                description: stored.description.clone(),
                status: GraphStatus::parse(&stored.status),
                input: stored.input.clone(),
                output: stored
                    .output
                    .clone()
                    .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text))),
                data: stored.data.clone(),
                input_schema: stored.input_schema.clone(),
                output_schema: stored.output_schema.clone(),
                error: None,
                member_ids: Vec::new(),
                live: false,
                started_at: stored.started_at,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        );
    }
    for node in nodes.values() {
        if let Some(group_id) = &node.group_id {
            let frame = groups
                .entry(group_id.clone())
                .or_insert_with(|| GraphFrame {
                    id: group_id.clone(),
                    name: node
                        .owner_name
                        .clone()
                        .map(|owner| format!("组 · {owner}"))
                        .unwrap_or_else(|| "智能体组".to_string()),
                    description: String::new(),
                    status: GraphStatus::Running,
                    input: None,
                    output: None,
                    data: None,
                    input_schema: None,
                    output_schema: None,
                    error: None,
                    member_ids: Vec::new(),
                    live: false,
                    started_at: None,
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                });
            frame.member_ids.push(node.id.clone());
        }
    }
    for task in tasks {
        if let Some(group_id) = &task.group_id
            && let Some(frame) = groups.get_mut(group_id)
            && !frame.member_ids.contains(&task.agent_id)
        {
            frame.member_ids.push(task.agent_id.clone());
        }
    }
    for frame in groups.values_mut() {
        frame.member_ids.sort();
        frame.member_ids.dedup();
        frame.live = frame
            .member_ids
            .iter()
            .any(|id| nodes.get(id).is_some_and(|node| node.live));
        frame.status = frame_status(frame, &nodes);
    }

    // Edges: persisted history + live parent/child relations.
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen: HashSet<(String, String, GraphEdgeKind)> = HashSet::new();
    for stored in &record.edges {
        let kind = GraphEdgeKind::parse(&stored.kind);
        let key = (stored.from.clone(), stored.to.clone(), kind);
        if seen.insert(key) {
            edges.push(GraphEdge {
                from: stored.from.clone(),
                to: stored.to.clone(),
                kind,
                label: stored.label.clone(),
                count: stored.count,
            });
        }
    }
    for node in nodes.values() {
        if node.is_main() {
            continue;
        }
        let parent = node.parent_id.clone().unwrap_or_else(|| "main".to_string());
        let target = node
            .group_id
            .clone()
            .filter(|group_id| groups.contains_key(group_id))
            .unwrap_or_else(|| node.id.clone());
        let key = (parent.clone(), target.clone(), GraphEdgeKind::Spawn);
        if seen.insert(key) {
            edges.push(GraphEdge {
                from: parent,
                to: target,
                kind: GraphEdgeKind::Spawn,
                label: "委派".to_string(),
                count: 1,
            });
        }
    }
    // Drop edges whose endpoints are neither a node nor a frame.
    let known: HashSet<&str> = nodes
        .keys()
        .map(String::as_str)
        .chain(groups.keys().map(String::as_str))
        .collect();
    edges.retain(|edge| known.contains(edge.from.as_str()) && known.contains(edge.to.as_str()));

    let member_ids: HashSet<String> = groups
        .values()
        .flat_map(|frame| frame.member_ids.iter().cloned())
        .collect();
    layout(&mut nodes, &mut groups, &member_ids);

    let mut bounds_x = 0.0_f32;
    let mut bounds_y = 0.0_f32;
    for node in nodes.values() {
        bounds_x = bounds_x.max(node.x + node.w);
        bounds_y = bounds_y.max(node.y + node.h);
    }
    for frame in groups.values() {
        bounds_x = bounds_x.max(frame.x + frame.w);
        bounds_y = bounds_y.max(frame.y + frame.h);
    }

    let nodes: Vec<GraphNode> = nodes.into_values().collect();
    let frames: Vec<GraphFrame> = groups.into_values().collect();
    GraphProjection {
        node_index: nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index))
            .collect(),
        frame_index: frames
            .iter()
            .enumerate()
            .map(|(index, frame)| (frame.id.clone(), index))
            .collect(),
        nodes,
        frames,
        edges,
        bounds: (bounds_x + 80.0, bounds_y + 80.0),
    }
}

fn main_node(session: &SessionState, running: bool) -> GraphNode {
    let last_user = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == effibuddy_agent_core::Role::User)
        .map(|message| message.text());
    let last_assistant = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == effibuddy_agent_core::Role::Assistant)
        .map(|message| message.text());
    GraphNode {
        id: "main".to_string(),
        name: "主智能体".to_string(),
        description: if session.title.trim().is_empty() {
            "与用户对话并调度子代理".to_string()
        } else {
            session.title.clone()
        },
        kind: AgentNodeKind::Main,
        profile: "main".to_string(),
        status: if running {
            GraphStatus::Running
        } else {
            GraphStatus::Idle
        },
        color: SubagentColor::Blue,
        visibility: AgentVisibility::Public,
        owner_id: None,
        owner_name: None,
        parent_id: None,
        group_id: None,
        depth: 0,
        can_spawn: true,
        read_only: false,
        session_id: Some(session.id.clone()),
        agent_id: None,
        tool_uses: 0,
        total_tokens: session.usage.total_tokens,
        duration_ms: 0,
        waiting_approval: false,
        task: last_user,
        output: last_assistant,
        data: None,
        input_schema: None,
        output_schema: None,
        started_at: None,
        finished_at: None,
        live: true,
        x: 0.0,
        y: 0.0,
        w: NODE_W,
        h: NODE_H,
    }
}

fn node_from_record(record: &AgentGraphNodeRecord, task: Option<&AgentTask>) -> GraphNode {
    let mut node = GraphNode {
        id: record.id.clone(),
        name: record.name.clone(),
        description: record.description.clone(),
        kind: record.kind,
        profile: record.profile.clone(),
        status: GraphStatus::parse(&record.status),
        color: color_for_profile(&record.profile),
        visibility: record.visibility,
        owner_id: record.owner_id.clone(),
        owner_name: record.owner_name.clone(),
        parent_id: record.parent_id.clone(),
        group_id: record.group_id.clone(),
        depth: record.depth,
        can_spawn: false,
        read_only: false,
        session_id: record.session_id.clone(),
        agent_id: Some(record.id.clone()),
        tool_uses: record.tool_uses,
        total_tokens: record.total_tokens,
        duration_ms: 0,
        waiting_approval: false,
        task: record.task.clone(),
        output: record.output.clone(),
        data: record.data.clone(),
        input_schema: record.input_schema.clone(),
        output_schema: record.output_schema.clone(),
        started_at: record.started_at,
        finished_at: record.finished_at,
        live: false,
        x: 0.0,
        y: 0.0,
        w: NODE_W,
        h: NODE_H,
    };
    if let Some(task) = task {
        overlay_task(&mut node, task);
    }
    node
}

fn node_from_task(task: &AgentTask) -> GraphNode {
    let mut node = GraphNode {
        id: task.agent_id.clone(),
        name: task.name.clone(),
        description: task.description.clone(),
        kind: AgentNodeKind::Agent,
        profile: task.profile.clone(),
        status: GraphStatus::from_subagent(task.status),
        color: task.color,
        visibility: task.visibility,
        owner_id: task.parent_agent_id.clone(),
        owner_name: task.parent_agent_name.clone(),
        parent_id: task.parent_agent_id.clone(),
        group_id: task.group_id.clone(),
        depth: task.depth,
        can_spawn: task.can_spawn,
        read_only: task.read_only,
        session_id: Some(task.child_session_id.clone()),
        agent_id: Some(task.agent_id.clone()),
        tool_uses: task.tool_uses,
        total_tokens: task.total_tokens,
        duration_ms: task.duration_ms,
        waiting_approval: task.waiting_approval,
        task: Some(task.task_prompt.clone()).filter(|text| !text.trim().is_empty()),
        output: task.report.clone(),
        data: task.data.clone(),
        input_schema: None,
        output_schema: task.output_schema.clone(),
        started_at: Some(
            chrono::DateTime::<Utc>::from(std::time::SystemTime::now())
                - chrono::Duration::from_std(task.started_at.elapsed()).unwrap_or_default(),
        ),
        finished_at: task.finished_at.and_then(|finished| {
            chrono::DateTime::<Utc>::from(std::time::SystemTime::now()).checked_sub_signed(
                chrono::Duration::from_std(finished.elapsed()).unwrap_or_default(),
            )
        }),
        live: true,
        x: 0.0,
        y: 0.0,
        w: NODE_W,
        h: NODE_H,
    };
    if task.status == SubagentStatus::Running && task.waiting_approval {
        node.status = GraphStatus::WaitingApproval;
    }
    node
}

fn overlay_task(node: &mut GraphNode, task: &AgentTask) {
    node.name = task.name.clone();
    node.description = task.description.clone();
    node.profile = task.profile.clone();
    node.status = GraphStatus::from_subagent(task.status);
    if task.status == SubagentStatus::Running && task.waiting_approval {
        node.status = GraphStatus::WaitingApproval;
    }
    node.color = task.color;
    node.visibility = task.visibility;
    node.owner_id = task.parent_agent_id.clone();
    node.owner_name = task.parent_agent_name.clone();
    node.parent_id = task.parent_agent_id.clone();
    node.group_id = task.group_id.clone();
    node.depth = task.depth;
    node.can_spawn = task.can_spawn;
    node.read_only = task.read_only;
    node.session_id = Some(task.child_session_id.clone());
    node.agent_id = Some(task.agent_id.clone());
    node.tool_uses = task.tool_uses;
    node.total_tokens = task.total_tokens;
    node.duration_ms = task.duration_ms;
    node.waiting_approval = task.waiting_approval;
    if !task.task_prompt.trim().is_empty() {
        node.task = Some(task.task_prompt.clone());
    }
    node.output = task.report.clone().or_else(|| node.output.clone());
    node.data = task.data.clone().or_else(|| node.data.clone());
    node.output_schema = task
        .output_schema
        .clone()
        .or_else(|| node.output_schema.clone());
    node.live = true;
    node.finished_at = task.finished_at.map(|_| Utc::now());
}

fn frame_status(frame: &GraphFrame, nodes: &BTreeMap<String, GraphNode>) -> GraphStatus {
    let mut status = GraphStatus::Completed;
    for id in &frame.member_ids {
        let Some(node) = nodes.get(id) else {
            continue;
        };
        match node.status {
            GraphStatus::Running | GraphStatus::WaitingApproval => return GraphStatus::Running,
            GraphStatus::Failed => status = GraphStatus::Failed,
            GraphStatus::Cancelled if status != GraphStatus::Failed => {
                status = GraphStatus::Cancelled
            }
            _ => {}
        }
    }
    status
}

fn layout(
    nodes: &mut BTreeMap<String, GraphNode>,
    groups: &mut BTreeMap<String, GraphFrame>,
    member_ids: &HashSet<String>,
) {
    // Compute depths for nodes that were persisted without one.
    let parents: HashMap<String, Option<String>> = nodes
        .iter()
        .map(|(id, node)| (id.clone(), node.parent_id.clone()))
        .collect();
    let mut depths: HashMap<String, usize> = nodes
        .iter()
        .map(|(id, node)| (id.clone(), node.depth))
        .collect();
    for id in nodes.keys() {
        let mut current = id.as_str();
        let mut depth = 0;
        let mut guard = 0;
        while let Some(parent) = parents.get(current).and_then(Option::as_deref) {
            if parent == current || guard > 64 {
                break;
            }
            depth += 1;
            current = parent;
            guard += 1;
        }
        depths.insert(id.clone(), depth.max(depths.get(id).copied().unwrap_or(0)));
    }
    for (id, node) in nodes.iter_mut() {
        if let Some(depth) = depths.get(id) {
            node.depth = *depth;
        }
    }

    let mut columns: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (id, node) in nodes.iter() {
        if member_ids.contains(id) {
            continue;
        }
        if node.is_main() {
            continue;
        }
        columns.entry(node.depth).or_default().push(id.clone());
    }
    let mut column_bottom: BTreeMap<usize, f32> = BTreeMap::new();
    if let Some(main) = nodes.get_mut("main") {
        main.x = 40.0;
        main.y = 40.0;
    }
    for (depth, ids) in columns.iter_mut() {
        ids.sort_by(|left, right| {
            let left_parent = nodes
                .get(left)
                .and_then(|node| node.parent_id.clone())
                .unwrap_or_default();
            let right_parent = nodes
                .get(right)
                .and_then(|node| node.parent_id.clone())
                .unwrap_or_default();
            left_parent.cmp(&right_parent).then_with(|| left.cmp(right))
        });
        let x = 40.0 + (*depth as f32) * (NODE_W + COL_GAP);
        let mut y = 40.0;
        for id in ids.iter() {
            if let Some(node) = nodes.get_mut(id) {
                node.x = x;
                node.y = y;
                y += NODE_H + ROW_GAP;
            }
        }
        column_bottom.insert(*depth, y);
    }
    if let Some(main) = nodes.get("main") {
        column_bottom.insert(0, main.y + main.h + ROW_GAP);
    }

    let mut groups_sorted: Vec<String> = groups.keys().cloned().collect();
    groups_sorted.sort_by(|left, right| {
        let left_depth = group_depth(groups.get(left), nodes);
        let right_depth = group_depth(groups.get(right), nodes);
        left_depth.cmp(&right_depth).then_with(|| left.cmp(right))
    });
    for group_id in groups_sorted {
        let depth = group_depth(groups.get(&group_id), nodes);
        let Some(frame) = groups.get_mut(&group_id) else {
            continue;
        };
        let cols = if frame.member_ids.len() <= 2 {
            frame.member_ids.len().max(1)
        } else {
            2
        };
        let rows = frame.member_ids.len().div_ceil(cols).max(1);
        frame.w = FRAME_PAD * 2.0
            + cols as f32 * NODE_W
            + (cols.saturating_sub(1)) as f32 * FRAME_MEMBER_GAP;
        frame.h = FRAME_HEADER
            + rows as f32 * NODE_H
            + (rows.saturating_sub(1)) as f32 * FRAME_MEMBER_GAP
            + FRAME_PAD;
        let x = 40.0 + (depth as f32) * (NODE_W + COL_GAP);
        let y = column_bottom.get(&depth).copied().unwrap_or(40.0);
        frame.x = x;
        frame.y = y;
        let member_ids = frame.member_ids.clone();
        for (index, member_id) in member_ids.iter().enumerate() {
            if let Some(node) = nodes.get_mut(member_id) {
                let col = index % cols;
                let row = index / cols;
                node.x = x + FRAME_PAD + col as f32 * (NODE_W + FRAME_MEMBER_GAP);
                node.y = y + FRAME_HEADER + row as f32 * (NODE_H + FRAME_MEMBER_GAP);
            }
        }
        column_bottom.insert(depth, y + frame.h + ROW_GAP);
    }
    // Members that have no frame (dangling group_id) fall back to the column.
    let mut orphan_y = column_bottom.values().copied().fold(40.0, f32::max);
    for (id, node) in nodes.iter_mut() {
        if member_ids.contains(id) && node.x == 0.0 && node.y == 0.0 {
            node.x = 40.0 + (node.depth as f32) * (NODE_W + COL_GAP);
            node.y = orphan_y;
            orphan_y += NODE_H + ROW_GAP;
        }
    }
}

fn group_depth(frame: Option<&GraphFrame>, nodes: &BTreeMap<String, GraphNode>) -> usize {
    frame
        .and_then(|frame| {
            frame
                .member_ids
                .iter()
                .filter_map(|id| nodes.get(id).map(|node| node.depth))
                .min()
        })
        .unwrap_or(1)
        .max(1)
}

pub fn color_for_profile(profile: &str) -> SubagentColor {
    match profile {
        "explore" => SubagentColor::Green,
        "reviewer" => SubagentColor::Purple,
        "general-purpose" => SubagentColor::Blue,
        _ => SubagentColor::Amber,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::SessionState;

    fn task(id: &str, parent: &str, depth: usize) -> AgentTask {
        AgentTask {
            agent_id: id.to_string(),
            name: id.to_string(),
            profile: "explore".to_string(),
            color: SubagentColor::Green,
            description: "task".to_string(),
            status: SubagentStatus::Completed,
            background: false,
            started_at: std::time::Instant::now(),
            finished_at: None,
            child_session_id: format!("sess_{id}"),
            parent_call_id: "call".to_string(),
            parent_session_id: "parent".to_string(),
            parent_agent_id: Some(parent.to_string()),
            parent_agent_name: Some(parent.to_string()),
            group_id: None,
            group_name: None,
            depth,
            visibility: AgentVisibility::Private,
            read_only: true,
            can_spawn: true,
            provider_id: None,
            model: "model".to_string(),
            shared: effibuddy_agent_core::new_shared_session(SessionState::new(
                std::env::temp_dir(),
                "model",
            )),
            report: Some("done".to_string()),
            error: None,
            data: None,
            tool_uses: 1,
            total_tokens: 10,
            duration_ms: 10,
            waiting_approval: false,
            restored: false,
            task_prompt: "do it".to_string(),
            output_schema: None,
        }
    }

    #[test]
    fn projection_places_children_and_groups_without_overlap() {
        let session = SessionState::new(std::env::temp_dir(), "model");
        let mut group_member = task("agent_group", "main", 1);
        group_member.group_id = Some("group_1".to_string());
        group_member.group_name = Some("research".to_string());
        let tasks = vec![task("agent_a", "main", 1), group_member];
        let projection = build_projection(&session, &tasks, false);
        assert!(projection.node("main").is_some());
        assert_eq!(projection.frames.len(), 1);
        let frame = &projection.frames[0];
        assert!(frame.w > NODE_W);
        let group_node = projection.node("agent_group").unwrap();
        assert!(group_node.x >= frame.x && group_node.y >= frame.y);
        assert!(
            projection
                .edges
                .iter()
                .any(|edge| edge.from == "main" && edge.to == "group_1")
        );
    }

    #[test]
    fn projection_indexes_cover_every_node_and_frame() {
        let session = SessionState::new(std::env::temp_dir(), "index");
        let mut group_member = task("agent_group", "main", 1);
        group_member.group_id = Some("group_1".to_string());
        group_member.group_name = Some("research".to_string());
        let tasks = vec![task("agent_a", "main", 1), group_member];
        let projection = build_projection(&session, &tasks, false);
        for node in &projection.nodes {
            assert!(
                projection.node(&node.id).is_some(),
                "missing node {}",
                node.id
            );
        }
        for frame in &projection.frames {
            assert!(
                projection.frame(&frame.id).is_some(),
                "missing frame {}",
                frame.id
            );
        }
        assert!(projection.node("definitely-missing").is_none());
    }

    #[test]
    fn private_and_public_visibility_survives_projection() {
        let session = SessionState::new(std::env::temp_dir(), "model");
        let mut private = task("agent_private", "main", 1);
        private.visibility = AgentVisibility::Private;
        let mut public = task("agent_public", "main", 1);
        public.visibility = AgentVisibility::Public;
        let projection = build_projection(&session, &[private, public], false);
        assert_eq!(
            projection.node("agent_private").unwrap().visibility,
            AgentVisibility::Private
        );
        assert_eq!(
            projection.node("agent_public").unwrap().visibility,
            AgentVisibility::Public
        );
    }
}
