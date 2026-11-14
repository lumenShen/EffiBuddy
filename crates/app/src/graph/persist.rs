//! Persist runtime events into the session's agent graph.
//!
//! The graph is deliberately event-sourced from the same lifecycle events the
//! UI already consumes: spawn, finish, message and workflow metadata. Nothing
//! here talks to GPUI, so the records survive a restart and can be tested in
//! isolation.

use chrono::Utc;
use effibuddy_agent_core::{
    AgentGraphNodeRecord, AgentGraphRecord, AgentMessage, AgentNodeKind, AgentVisibility,
    SubagentReport, SubagentStatus,
};
use serde_json::Value;

use crate::events::SubagentSpawnedInfo;

pub fn record_spawn(graph: &mut AgentGraphRecord, info: &SubagentSpawnedInfo) {
    let parent_id = info
        .parent_agent_id
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let parent_name = info
        .parent_agent_name
        .clone()
        .unwrap_or_else(|| "main".to_string());
    graph.upsert_node(AgentGraphNodeRecord {
        id: info.agent_id.clone(),
        name: info.name.clone(),
        description: info.description.clone(),
        kind: AgentNodeKind::Agent,
        profile: info.profile.clone(),
        parent_id: Some(parent_id.clone()),
        parent_name: Some(parent_name.clone()),
        owner_id: Some(parent_id.clone()),
        owner_name: Some(parent_name.clone()),
        visibility: info.visibility,
        group_id: info.group_id.clone(),
        group_name: info.group_name.clone(),
        depth: info.depth,
        status: SubagentStatus::Running.label().to_string(),
        session_id: Some(info.child_session_id.clone()),
        tool_uses: 0,
        total_tokens: 0,
        task: Some(info.task_prompt.clone()),
        output: None,
        data: None,
        input: None,
        input_schema: None,
        output_schema: info.output_schema.clone(),
        started_at: Some(Utc::now()),
        finished_at: None,
    });
    if let Some(group_id) = &info.group_id {
        ensure_group(
            graph,
            group_id,
            info.group_name.as_deref(),
            &parent_id,
            &parent_name,
        );
        graph.upsert_edge(&parent_id, group_id, "spawn", "委派");
    } else {
        graph.upsert_edge(&parent_id, &info.agent_id, "spawn", "委派");
    }
}

fn ensure_group(
    graph: &mut AgentGraphRecord,
    group_id: &str,
    group_name: Option<&str>,
    parent_id: &str,
    parent_name: &str,
) {
    if graph.node(group_id).is_some() {
        return;
    }
    graph.upsert_node(AgentGraphNodeRecord {
        id: group_id.to_string(),
        name: group_name.unwrap_or(group_id).to_string(),
        description: "智能体组".to_string(),
        kind: AgentNodeKind::Group,
        profile: "group".to_string(),
        parent_id: Some(parent_id.to_string()),
        parent_name: Some(parent_name.to_string()),
        owner_id: Some(parent_id.to_string()),
        owner_name: Some(parent_name.to_string()),
        visibility: AgentVisibility::Public,
        group_id: None,
        group_name: None,
        depth: 1,
        status: SubagentStatus::Running.label().to_string(),
        session_id: None,
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
    });
}

pub fn record_finish(graph: &mut AgentGraphRecord, report: &SubagentReport) {
    let status = report.status.label().to_string();
    if let Some(node) = graph.node_mut(&report.agent_id) {
        node.name = report.name.clone();
        node.status = status.clone();
        node.output = Some(report.report.clone());
        node.data = report.data.clone();
        node.tool_uses = report.tool_uses;
        node.total_tokens = report.usage.total_tokens;
        node.finished_at = Some(Utc::now());
    }
    if let Some(group_id) = &report.group_id {
        refresh_group_status(graph, group_id);
    }
}

pub fn record_message(
    graph: &mut AgentGraphRecord,
    message: &AgentMessage,
    queued: bool,
    woke: bool,
) {
    graph.push_message(message.clone());
    let delivery = if woke {
        "已唤醒"
    } else if queued {
        "已排队"
    } else {
        "已送达"
    };
    let label = format!(
        "{} · {} · {}",
        message.preview(48),
        message.kind.label(),
        delivery
    );
    graph.upsert_edge(&message.from_id, &message.to_id, "message", &label);
}

/// Parse the `workflow` metadata emitted by `workflow_run`. This is the bridge
/// between the tool result and the graph's group frame: inputs, outputs,
/// schemas, member ids and the internal data edges all come from here.
pub fn record_workflow_metadata(graph: &mut AgentGraphRecord, metadata: &Value) {
    let Some(workflow) = metadata.get("workflow") else {
        return;
    };
    let Some(id) = workflow.get("id").and_then(Value::as_str) else {
        return;
    };
    let name = workflow
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("智能体组");
    let parent_id = "main";
    ensure_group(graph, id, Some(name), parent_id, "main");
    if let Some(node) = graph.node_mut(id) {
        node.name = name.to_string();
        node.description = workflow
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        node.status = match workflow.get("status").and_then(Value::as_str) {
            Some("completed") => SubagentStatus::Completed.label().to_string(),
            Some("failed") => SubagentStatus::Failed.label().to_string(),
            _ => SubagentStatus::Running.label().to_string(),
        };
        node.input = workflow.get("input").cloned();
        node.output = workflow
            .get("output")
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_default());
        node.data = workflow.get("output").cloned();
        node.input_schema = workflow.get("input_schema").cloned();
        node.output_schema = workflow.get("output_schema").cloned();
        node.task = workflow
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        if node.status == SubagentStatus::Completed.label() {
            node.finished_at = Some(Utc::now());
        }
    }

    let mut workflow_nodes: Vec<&Value> = Vec::new();
    if let Some(nodes) = workflow.get("nodes").and_then(Value::as_array) {
        workflow_nodes.extend(nodes.iter());
    } else if let Some(runs) = workflow.get("runs").and_then(Value::as_array) {
        for run in runs {
            if let Some(nodes) = run.get("nodes").and_then(Value::as_array) {
                workflow_nodes.extend(nodes.iter());
            }
        }
    }
    let mut node_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in &workflow_nodes {
        let Some(agent_id) = entry.get("agent_id").and_then(Value::as_str) else {
            continue;
        };
        let workflow_node_id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(agent_id)
            .to_string();
        node_map.insert(workflow_node_id, agent_id.to_string());
        if let Some(node) = graph.node_mut(agent_id) {
            node.group_id = Some(id.to_string());
            node.group_name = Some(name.to_string());
            node.status = match entry.get("status").and_then(Value::as_str) {
                Some("completed") => SubagentStatus::Completed.label().to_string(),
                Some("failed") => SubagentStatus::Failed.label().to_string(),
                _ => node.status.clone(),
            };
            if let Some(data) = entry.get("data") {
                node.data = Some(data.clone());
                node.output =
                    Some(serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string()));
            }
            if let Some(tool_uses) = entry.get("tool_uses").and_then(Value::as_u64) {
                node.tool_uses = tool_uses as usize;
            }
            node.finished_at = Some(Utc::now());
        }
    }
    if let Some(edges) = workflow.get("edges").and_then(Value::as_array) {
        for edge in edges {
            let Some(from) = edge.get("from").and_then(Value::as_str) else {
                continue;
            };
            let Some(to) = edge.get("to").and_then(Value::as_str) else {
                continue;
            };
            if to == "@output" {
                continue;
            }
            let Some(from_agent) = node_map.get(from) else {
                continue;
            };
            let Some(to_agent) = node_map.get(to) else {
                continue;
            };
            graph.upsert_edge(from_agent, to_agent, "data", "数据");
        }
    }
}

fn refresh_group_status(graph: &mut AgentGraphRecord, group_id: &str) {
    let mut status = SubagentStatus::Completed;
    for node in graph
        .nodes
        .iter()
        .filter(|node| node.group_id.as_deref() == Some(group_id))
    {
        match node.status.as_str() {
            "运行中" => status = SubagentStatus::Running,
            "失败" => {
                status = SubagentStatus::Failed;
                break;
            }
            "已停止" if status != SubagentStatus::Failed => status = SubagentStatus::Cancelled,
            _ => {}
        }
    }
    if let Some(group) = graph.node_mut(group_id) {
        group.status = status.label().to_string();
        if status != SubagentStatus::Running {
            group.finished_at = Some(Utc::now());
        }
    }
}

pub fn save_viewport(graph: &mut AgentGraphRecord, pan: [f32; 2], zoom: f32) {
    graph.pan.x = pan[0];
    graph.pan.y = pan[1];
    graph.zoom = zoom.clamp(0.35, 2.2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{AgentGraphRecord, AgentMessageKind, Usage};
    use serde_json::json;

    fn message() -> AgentMessage {
        AgentMessage {
            id: "msg_1".to_string(),
            from_id: "main".to_string(),
            from_name: "main".to_string(),
            to_id: "agent_a".to_string(),
            to_name: "scout".to_string(),
            kind: AgentMessageKind::Data,
            text: "findings".to_string(),
            data: Some(json!({"ok": true})),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn messages_create_edges_and_are_retained() {
        let mut graph = AgentGraphRecord::default();
        record_message(&mut graph, &message(), false, true);
        record_message(&mut graph, &message(), true, false);
        assert_eq!(graph.messages.len(), 2);
        let edge = graph
            .edges
            .iter()
            .find(|edge| edge.kind == "message")
            .unwrap();
        assert_eq!(edge.count, 2);
        assert!(edge.label.contains("已排队"));
    }

    #[test]
    fn workflow_metadata_builds_group_and_data_edges() {
        let mut graph = AgentGraphRecord::default();
        graph.upsert_node(AgentGraphNodeRecord {
            id: "agent_research".to_string(),
            name: "research".to_string(),
            description: String::new(),
            kind: AgentNodeKind::Agent,
            profile: "explore".to_string(),
            parent_id: Some("main".to_string()),
            parent_name: Some("main".to_string()),
            owner_id: Some("main".to_string()),
            owner_name: Some("main".to_string()),
            visibility: AgentVisibility::Public,
            group_id: Some("group_1".to_string()),
            group_name: Some("audit".to_string()),
            depth: 1,
            status: "运行中".to_string(),
            session_id: Some("sess_1".to_string()),
            tool_uses: 0,
            total_tokens: 0,
            task: None,
            output: None,
            data: None,
            input: None,
            input_schema: None,
            output_schema: None,
            started_at: None,
            finished_at: None,
        });
        record_workflow_metadata(
            &mut graph,
            &json!({
                "workflow": {
                    "id": "group_1",
                    "name": "audit",
                    "status": "completed",
                    "input": {"topic": "x"},
                    "output": {"verdict": "pass"},
                    "nodes": [
                        {"id": "research", "agent_id": "agent_research", "status": "completed", "data": {"facts": []}},
                        {"id": "review", "agent_id": "agent_review", "status": "completed", "data": {"verdict": "pass"}}
                    ],
                    "edges": [{"from": "research", "to": "review"}]
                }
            }),
        );
        let group = graph.node("group_1").unwrap();
        assert_eq!(group.kind, AgentNodeKind::Group);
        assert_eq!(group.status, "已完成");
        assert!(group.output.is_some());
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.from == "agent_research" && edge.to == "agent_review")
        );
    }

    #[test]
    fn finish_updates_status_and_usage() {
        let mut graph = AgentGraphRecord::default();
        graph.upsert_node(AgentGraphNodeRecord {
            id: "agent_a".to_string(),
            name: "scout".to_string(),
            description: String::new(),
            kind: AgentNodeKind::Agent,
            profile: "explore".to_string(),
            parent_id: None,
            parent_name: None,
            owner_id: None,
            owner_name: None,
            visibility: AgentVisibility::Private,
            group_id: None,
            group_name: None,
            depth: 1,
            status: "运行中".to_string(),
            session_id: None,
            tool_uses: 0,
            total_tokens: 0,
            task: None,
            output: None,
            data: None,
            input: None,
            input_schema: None,
            output_schema: None,
            started_at: None,
            finished_at: None,
        });
        let report = SubagentReport {
            agent_id: "agent_a".to_string(),
            name: "scout".to_string(),
            profile: "explore".to_string(),
            color: effibuddy_agent_core::SubagentColor::Green,
            description: String::new(),
            child_session_id: "sess".to_string(),
            parent_call_id: "call".to_string(),
            parent_agent_id: Some("main".to_string()),
            group_id: None,
            group_name: None,
            visibility: AgentVisibility::Private,
            background: false,
            wake: false,
            status: SubagentStatus::Completed,
            report: "done".to_string(),
            data: Some(json!(1)),
            error: None,
            tool_uses: 2,
            duration_ms: 10,
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                last_prompt_tokens: 5,
            },
        };
        record_finish(&mut graph, &report);
        let node = graph.node("agent_a").unwrap();
        assert_eq!(node.status, "已完成");
        assert_eq!(node.total_tokens, 10);
        assert_eq!(node.data, Some(json!(1)));
    }
}
