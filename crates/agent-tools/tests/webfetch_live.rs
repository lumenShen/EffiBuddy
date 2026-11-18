//! Live `webfetch` checks against the real internet.
//!
//! Everything here is `#[ignore]` by default — CI has no business depending on
//! public sites. Run manually when changing the extractor or the request path:
//!
//! ```text
//! cargo test -p effibuddy-agent-tools --test webfetch_live -- --ignored --nocapture
//! ```
//!
//! Assertions stay loose (title text, markers, budget) because the pages are
//! not ours; the point is that TLS, gzip, redirects and extraction still work
//! end to end against real servers.

use effibuddy_agent_core::{Tool, ToolContext};
use effibuddy_agent_tools::WebFetchTool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn context() -> ToolContext {
    ToolContext {
        cwd: std::env::temp_dir(),
        additional_workspaces: Vec::new(),
        session_id: "live".to_string(),
        agent_id: "main".to_string(),
        agent_name: None,
        agent_depth: 0,
        call_id: "call-live".to_string(),
        cancel: CancellationToken::new(),
        nested: None,
        subagents: None,
        checkpoint_sink: None,
    }
}

async fn fetch(url: &str, max_chars: u64) -> String {
    let tool = WebFetchTool::new();
    let output = tool
        .execute(context(), json!({ "url": url, "max_chars": max_chars }))
        .await
        .unwrap_or_else(|error| panic!("{url}: {}", error.message));
    println!("=== {url} ===\n{}\n", output.content);
    output.content
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn reads_a_documentation_page() {
    let content = fetch(
        "https://doc.rust-lang.org/book/ch01-02-hello-world.html",
        8_000,
    )
    .await;
    assert!(content.contains("Status: 200"), "{content}");
    assert!(content.contains("Hello, World!"), "{content}");
    assert!(content.contains("Title:"), "{content}");
    // The page is code-heavy: the fenced block must survive extraction.
    assert!(content.contains("```"), "{content}");
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn reads_json() {
    let content = fetch("https://api.github.com/repos/rust-lang/rust", 4_000).await;
    assert!(content.contains("Status: 200"), "{content}");
    assert!(
        content.contains("\"full_name\": \"rust-lang/rust\""),
        "{content}"
    );
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn reports_http_errors() {
    let tool = WebFetchTool::new();
    let error = tool
        .execute(
            context(),
            json!({"url": "https://example.com/definitely-not-here"}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("HTTP 404"), "{}", error.message);
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn follows_real_redirects() {
    // `http://` → `https://` is the most common redirect in the wild.
    let content = fetch("http://github.com/", 4_000).await;
    assert!(content.contains("Status: 200"), "{content}");
    assert!(content.contains("URL: https://github.com/"), "{content}");
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn public_only_still_reads_public_hosts() {
    let tool = WebFetchTool::new().public_only(true);
    let output = tool
        .execute(context(), json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    assert!(
        output.content.contains("Example Domain"),
        "{}",
        output.content
    );
}

/// The full harness path: registered builtin tool → approval gate → real HTTP
/// request → observation in the transcript. This is what the app actually does
/// when the model calls `webfetch`.
#[tokio::test]
#[ignore = "needs internet access"]
async fn harness_runs_a_webfetch_tool_call() {
    use effibuddy_agent_core::testing::ScriptedProvider;
    use effibuddy_agent_core::{
        AgentHarness, AutoApprovalGate, Completion, HarnessConfig, RunOutcome, SessionState,
        ToolCall, lock_session, new_shared_session,
    };
    use effibuddy_agent_tools::builtin_tools;
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let registry = builtin_tools(directory.path()).unwrap();
    assert!(
        registry.names().iter().any(|name| name == "webfetch"),
        "webfetch must be in the default registry"
    );

    let session = new_shared_session(SessionState::new(
        directory.path().to_path_buf(),
        "test-model",
    ));
    let provider = Arc::new(ScriptedProvider::new(vec![
        Completion::default().with_tool_call(ToolCall::new(
            "webfetch",
            json!({"url": "https://example.com/"}),
        )),
        Completion::text("done"),
    ]));
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let harness = AgentHarness::new(
        session.clone(),
        provider,
        Arc::new(registry),
        Arc::new(AutoApprovalGate),
        HarnessConfig::default(),
        events,
    );

    let outcome = harness.run_turn("fetch example.com").await.unwrap();
    assert_eq!(outcome, RunOutcome::Completed);

    let session = lock_session(&session);
    let result = session.messages[2].tool_result().expect("tool result");
    println!("--- observation ---\n{}", result.content);
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Example Domain"),
        "{}",
        result.content
    );
    assert!(result.content.contains("Status: 200"), "{}", result.content);
}
