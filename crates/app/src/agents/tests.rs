//! Runtime tests: lifecycle, naming, structured contracts, messaging and
//! private/public visibility.

use std::sync::Arc;
use std::time::Duration;

use effibuddy_agent_core::testing::ScriptedProvider;
use effibuddy_agent_core::{
    AgentVisibility, Completion, HarnessConfig, SubagentRequest, SubagentSpawner, SubagentStatus,
};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::events::AppEvent;
use crate::store::SessionStore;

use super::runtime::SubagentService;

fn request(workspace: &std::path::Path, background: bool) -> SubagentRequest {
    SubagentRequest {
        profile: "explore".to_string(),
        description: "find the parser".to_string(),
        prompt: "Find the parser entry point.".to_string(),
        background,
        parent_session_id: "parent-session".to_string(),
        parent_call_id: "call-parent".to_string(),
        workspace: workspace.to_path_buf(),
        additional_workspaces: Vec::new(),
        agent_name: None,
        parent_agent_id: Some("main".to_string()),
        parent_agent_name: Some("main".to_string()),
        depth: 1,
        system_prompt: None,
        tools: None,
        allow_spawn: true,
        read_only: Some(true),
        output_schema: None,
        visibility: AgentVisibility::Private,
        group_id: None,
        group_name: None,
    }
}

fn service(
    directory: &tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    events: UnboundedSender<AppEvent>,
) -> SubagentService {
    SubagentService::new(
        provider,
        "parent-session".to_string(),
        "deepseek".to_string(),
        "test-model".to_string(),
        None,
        HarnessConfig::default(),
        String::new(),
        SessionStore::with_directory(directory.path().join("sessions")),
        events,
        tokio::runtime::Handle::current(),
        directory.path().to_path_buf(),
        Arc::new(effibuddy_agent_core::ModelConfigFile::with_builtins()),
    )
}

async fn wait_for_finish(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> effibuddy_agent_core::SubagentReport {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = rx.recv().await {
            if let AppEvent::SubagentFinished { report, .. } = event {
                return report;
            }
        }
        panic!("event channel closed before completion");
    })
    .await
    .expect("agent finished in time")
}

#[tokio::test]
async fn foreground_child_runs_persists_and_emits_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Completion::text(
        "The parser lives in src/parser.rs.",
    )]));
    let (tx, mut rx) = unbounded_channel();
    let service = service(&directory, provider, tx);

    let report = service
        .run(
            request(directory.path(), false),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    assert_eq!(report.status, SubagentStatus::Completed);
    assert_eq!(report.report, "The parser lives in src/parser.rs.");
    let store = SessionStore::with_directory(directory.path().join("sessions"));
    let child = store.load(&report.child_session_id).expect("child saved");
    assert_eq!(child.parent_id.as_deref(), Some("parent-session"));
    assert_eq!(child.agent_name.as_deref(), Some(report.name.as_str()));
    assert_eq!(child.provider_id.as_deref(), Some("deepseek"));

    let mut spawned = false;
    let mut finished = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::SubagentSpawned { info, .. } => {
                spawned = true;
                assert_eq!(info.name, report.name);
                assert_eq!(info.depth, 1);
                assert_eq!(info.visibility, AgentVisibility::Private);
            }
            AppEvent::SubagentFinished {
                report: finished_report,
                ..
            } => {
                finished = true;
                assert_eq!(finished_report.name, report.name);
            }
            _ => {}
        }
    }
    assert!(spawned, "spawn event missing");
    assert!(finished, "finish event missing");
}

#[tokio::test]
async fn background_launch_returns_then_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Completion::text("done")]));
    let (tx, mut rx) = unbounded_channel();
    let service = service(&directory, provider, tx);

    let launch = service
        .launch(request(directory.path(), true))
        .expect("background launch");
    assert!(!launch.agent_id.is_empty());
    assert!(!launch.agent_name.is_empty());

    let finished = wait_for_finish(&mut rx).await;
    assert_eq!(finished.status, SubagentStatus::Completed);
    assert!(finished.background);
}

#[tokio::test]
async fn stopping_a_background_agent_reports_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Completion::text("first")]));
    let (tx, mut rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let launch = service
        .launch(request(directory.path(), true))
        .expect("launch");
    service.stop(&launch.agent_id);

    let report = wait_for_finish(&mut rx).await;
    assert!(matches!(
        report.status,
        SubagentStatus::Cancelled | SubagentStatus::Completed
    ));
}

#[tokio::test]
async fn custom_agents_get_unique_names_and_structured_contracts() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Completion::text("{\"verdict\":\"pass\"}"),
        Completion::text("{\"verdict\":\"pass\"}"),
    ]));
    let (tx, _rx) = unbounded_channel();
    let service = service(&directory, provider, tx);

    let mut first = request(directory.path(), false);
    first.profile = "security-auditor".to_string();
    first.agent_name = Some("Auditor".to_string());
    first.system_prompt = Some("Audit security.".to_string());
    first.tools = Some(vec!["read_file".to_string()]);
    first.output_schema = Some(serde_json::json!({
        "type": "object",
        "required": ["verdict"],
        "properties": {"verdict": {"enum": ["pass", "fail"]}}
    }));
    let first_report = service
        .run(first, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(first_report.status, SubagentStatus::Completed);
    assert_eq!(first_report.name, "auditor");
    assert_eq!(
        first_report.data,
        Some(serde_json::json!({"verdict": "pass"}))
    );

    let mut second = request(directory.path(), false);
    second.profile = "security-auditor".to_string();
    second.agent_name = Some("Auditor".to_string());
    second.system_prompt = Some("Audit security.".to_string());
    let second_report = service
        .run(second, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(second_report.name, "auditor-2");
}

#[tokio::test]
async fn structured_output_failure_is_reported_as_failed() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Completion::text("not json")]));
    let (tx, _rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let mut request = request(directory.path(), false);
    request.output_schema = Some(serde_json::json!({
        "type": "object",
        "required": ["verdict"]
    }));
    let report = service
        .run(request, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(report.status, SubagentStatus::Failed);
    assert!(report.error.unwrap().contains("not valid JSON"));
}

#[tokio::test]
async fn messaging_to_an_idle_agent_wakes_it() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Completion::text("first"),
        Completion::text("second"),
    ]));
    let (tx, mut rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let launch = service
        .launch(request(directory.path(), true))
        .expect("launch");
    let first = wait_for_finish(&mut rx).await;
    assert!(!first.wake);

    let outcome = service
        .send_agent_message(
            "parent-session",
            &launch.agent_name,
            effibuddy_agent_core::AgentMessageKind::Message,
            "follow up".to_string(),
            None,
        )
        .expect("message delivered");
    assert!(outcome.delivered);
    assert!(outcome.woke, "idle agent should be woken");

    let second = wait_for_finish(&mut rx).await;
    assert!(second.wake);
    assert_eq!(second.status, SubagentStatus::Completed);
}

#[tokio::test]
async fn private_agents_are_only_addressable_by_their_owner() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Completion::text("owner"),
        Completion::text("sibling"),
    ]));
    let (tx, _rx) = unbounded_channel();
    let service = service(&directory, provider, tx);

    let mut owner_request = request(directory.path(), true);
    owner_request.agent_name = Some("owner-agent".to_string());
    owner_request.visibility = AgentVisibility::Public;
    let owner = service.launch(owner_request).expect("owner launched");

    let mut private_request = request(directory.path(), true);
    private_request.agent_name = Some("private-helper".to_string());
    private_request.parent_agent_id = Some(owner.agent_id.clone());
    private_request.parent_agent_name = Some(owner.agent_name.clone());
    private_request.depth = 2;
    private_request.visibility = AgentVisibility::Private;
    let private = service
        .launch(private_request)
        .expect("private child launched");

    let error = service
        .send_agent_message(
            "parent-session",
            &private.agent_name,
            effibuddy_agent_core::AgentMessageKind::Message,
            "hello".to_string(),
            None,
        )
        .unwrap_err();
    assert!(error.message.contains("private"), "{}", error.message);

    let visible_to_main = service.visible_agents("main");
    assert!(
        visible_to_main
            .iter()
            .all(|agent| agent.name != private.agent_name),
        "private agent leaked into main's address book"
    );
    let visible_to_owner = service.visible_agents(&owner.agent_id);
    assert!(
        visible_to_owner
            .iter()
            .any(|agent| agent.name == private.agent_name),
        "owner must see its private agent"
    );
}

#[tokio::test]
async fn read_only_parent_cannot_create_a_mutating_child() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Completion::text("parent"),
        Completion::text("child"),
    ]));
    let (tx, _rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let mut parent_request = request(directory.path(), true);
    parent_request.agent_name = Some("read-only-parent".to_string());
    let parent = service.launch(parent_request).expect("parent launched");

    let mut child_request = request(directory.path(), false);
    child_request.agent_name = Some("child".to_string());
    child_request.parent_agent_id = Some(parent.agent_id.clone());
    child_request.parent_agent_name = Some(parent.agent_name.clone());
    child_request.depth = 2;
    child_request.read_only = Some(false);
    child_request.visibility = AgentVisibility::Public;
    let child = service.launch(child_request).expect("child launched");
    assert_eq!(
        service.record_read_only(&child.agent_id),
        Some(true),
        "read-only parent escalated a child"
    );
}

#[tokio::test]
async fn messaging_to_main_is_forwarded_to_the_ui() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let (tx, mut rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let outcome = service
        .send_agent_message(
            "parent-session",
            "main",
            effibuddy_agent_core::AgentMessageKind::Data,
            "status".to_string(),
            Some(serde_json::json!({"ok": true})),
        )
        .unwrap();
    assert!(outcome.woke);
    let event = rx.try_recv().expect("event");
    match event {
        AppEvent::AgentMessageToMain { message, .. } => {
            assert_eq!(message.to_name, "main");
            assert_eq!(message.data, Some(serde_json::json!({"ok": true})));
        }
        _ => panic!("unexpected event"),
    }
}

#[tokio::test]
async fn agents_lists_main_children_and_groups() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Completion::text("done")]));
    let (tx, _rx) = unbounded_channel();
    let service = service(&directory, provider, tx);
    let mut request = request(directory.path(), true);
    request.group_id = Some("group_test".to_string());
    request.group_name = Some("audit".to_string());
    service.launch(request).expect("launch");
    let agents = service.agents();
    assert!(agents.iter().any(|agent| agent.id == "main"));
    assert!(
        agents
            .iter()
            .any(|agent| agent.kind == effibuddy_agent_core::AgentNodeKind::Group)
    );
    assert!(
        agents
            .iter()
            .any(|agent| agent.kind == effibuddy_agent_core::AgentNodeKind::Agent)
    );
}
