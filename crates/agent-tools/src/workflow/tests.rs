use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use effibuddy_agent_core::{
    ApprovalDecision, SubagentLaunch, SubagentProfile, SubagentReport, SubagentRequest,
    SubagentSpawner, SubagentStatus, Tool, ToolContext, ToolError, Usage, WorkflowNodeSpec,
    WorkflowSpec, builtin_subagent_profiles,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{WorkflowTool, final_output};

struct RecordingSpawner {
    calls: Mutex<Vec<SubagentRequest>>,
}

#[async_trait::async_trait]
impl SubagentSpawner for RecordingSpawner {
    fn profiles(&self) -> Vec<SubagentProfile> {
        builtin_subagent_profiles()
    }

    async fn run(&self, request: SubagentRequest, _cancel: CancellationToken) -> SubagentReport {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request.clone());
            calls.len()
        };
        let data = if request.agent_name.as_deref() == Some("research") {
            Some(json!({"facts": ["a", "b"]}))
        } else {
            Some(json!({"verdict": "pass"}))
        };
        SubagentReport {
            agent_id: format!("agent_{index}"),
            visibility: request.visibility,
            name: request
                .agent_name
                .clone()
                .unwrap_or_else(|| "node".to_string()),
            profile: request.profile.clone(),
            color: effibuddy_agent_core::SubagentColor::Green,
            description: request.description.clone(),
            child_session_id: format!("sess_{index}"),
            parent_call_id: request.parent_call_id.clone(),
            parent_agent_id: request.parent_agent_id.clone(),
            group_id: request.group_id.clone(),
            group_name: request.group_name.clone(),
            background: false,
            wake: false,
            status: SubagentStatus::Completed,
            report: serde_json::to_string(&data).unwrap(),
            data,
            error: None,
            tool_uses: 1,
            duration_ms: 5,
            usage: Usage::default(),
        }
    }

    fn launch(&self, _request: SubagentRequest) -> Result<SubagentLaunch, ToolError> {
        Err(ToolError::new("not supported in tests"))
    }

    fn stop(&self, _agent_id: &str) {}

    fn respond_approval(
        &self,
        _agent_id: &str,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> bool {
        true
    }
}

fn context(spawner: Arc<RecordingSpawner>) -> ToolContext {
    ToolContext {
        cwd: std::env::temp_dir(),
        additional_workspaces: Vec::new(),
        session_id: "main-session".to_string(),
        agent_id: "main".to_string(),
        agent_name: None,
        agent_depth: 0,
        call_id: "call-group".to_string(),
        cancel: CancellationToken::new(),
        nested: None,
        subagents: Some(spawner),
        checkpoint_sink: None,
    }
}

#[tokio::test]
async fn js_definition_runs_a_typed_dag() {
    let spawner = Arc::new(RecordingSpawner {
        calls: Mutex::new(Vec::new()),
    });
    let output = WorkflowTool::new()
        .execute(
            context(spawner.clone()),
            json!({
                "code": r#"
const g = workflow.group({
  name: "research-to-review",
  input: { type: "object", required: ["topic"], properties: { topic: { type: "string" } } },
  output: { type: "object", required: ["verdict"], properties: { verdict: { type: "string" } } }
});
g.agent("research", {
  profile: "explore",
  prompt: "Research {{input.topic}}",
  output: { type: "object", required: ["facts"] }
});
g.agent("review", {
  profile: "reviewer",
  prompt: "Review {{research.facts}}",
  output: { type: "object", required: ["verdict"] }
});
g.link("research", "review");
return g.spec;
"#,
                "input": {"topic": "agents"}
            }),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    let metadata = output.metadata.expect("workflow metadata");
    assert_eq!(metadata["workflow"]["status"], "completed");
    assert_eq!(metadata["workflow"]["output"]["verdict"], "pass");
    let calls = spawner.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].group_id.as_deref(),
        metadata["workflow"]["id"].as_str()
    );
    assert!(calls.iter().all(|call| call.depth == 1));
}

#[tokio::test]
async fn batch_inputs_run_independently() {
    let spawner = Arc::new(RecordingSpawner {
        calls: Mutex::new(Vec::new()),
    });
    let output = WorkflowTool::new()
        .execute(
            context(spawner.clone()),
            json!({
                "spec": {
                    "name": "single",
                    "nodes": [{
                        "id": "research",
                        "name": "research",
                        "profile": "explore",
                        "prompt": "Research {{input.topic}}"
                    }],
                    "edges": []
                },
                "inputs": [{"topic": "a"}, {"topic": "b"}]
            }),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    let metadata = output.metadata.unwrap();
    assert_eq!(metadata["workflow"]["runs"].as_array().unwrap().len(), 2);
    assert_eq!(metadata["workflow"]["nodes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn input_contract_is_enforced_before_any_agent_runs() {
    let spawner = Arc::new(RecordingSpawner {
        calls: Mutex::new(Vec::new()),
    });
    let error = WorkflowTool::new()
        .execute(
            context(spawner.clone()),
            json!({
                "spec": {
                    "name": "typed",
                    "input_schema": {
                        "type": "object",
                        "required": ["topic"],
                        "properties": {"topic": {"type": "string"}}
                    },
                    "nodes": [{"id": "a", "profile": "general-purpose"}]
                },
                "input": {}
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error.message.contains("missing required property"),
        "{}",
        error.message
    );
    assert!(spawner.calls.lock().unwrap().is_empty());
}

#[test]
fn output_uses_sink_node_or_keyed_object() {
    let spec = WorkflowSpec {
        name: "two-sinks".to_string(),
        description: String::new(),
        input_schema: None,
        output_schema: None,
        nodes: vec![node("a"), node("b")],
        edges: vec![],
        max_parallel: 2,
    };
    let mut outputs = BTreeMap::new();
    outputs.insert("a".to_string(), json!(1));
    outputs.insert("b".to_string(), json!(2));
    assert_eq!(final_output(&spec, &outputs), json!({"a": 1, "b": 2}));
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
        visibility: effibuddy_agent_core::AgentVisibility::Private,
    }
}
