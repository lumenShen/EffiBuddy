//! Live DeepSeek integration tests.
//!
//! These tests hit the real API and are `#[ignore]` by default. Run them with:
//!
//! ```text
//! cargo test -p effibuddy-agent-tools --test deepseek_live -- --ignored --nocapture
//! ```
//!
//! The API key is read from `DEEPSEEK_API_KEY` or from
//! `deepseekAPIKey.env` at the repository root. The key is never printed.
//! The model defaults to `deepseek-flash` and can be overridden with
//! `DEEPSEEK_MODEL`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use effibuddy_agent_core::{
    AgentHarness, AutoApprovalGate, ChatMessage, ChatRequest, FinishReason, HarnessConfig,
    ModelProvider, OpenAiCompatConfig, OpenAiCompatProvider, RunOutcome, SessionState, Tool,
    ToolContext, ToolError, ToolOutput, ToolRegistry, lock_session, new_shared_session,
};
use effibuddy_agent_tools::{RunJsTool, builtin_tools};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn key_file() -> PathBuf {
    std::env::var("DEEPSEEK_API_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../deepseekAPIKey.env")
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from("../../deepseekAPIKey.env"))
        })
}

fn api_key() -> String {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        if !key.trim().is_empty() {
            return key.trim().to_string();
        }
    }
    let path = key_file();
    let key = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read DeepSeek key from {}: {error}. Set DEEPSEEK_API_KEY.",
            path.display()
        )
    });
    let key = key.trim().to_string();
    assert!(!key.is_empty(), "DeepSeek key file is empty");
    key
}

fn model() -> String {
    std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".to_string())
}

fn provider() -> OpenAiCompatProvider {
    let mut config = OpenAiCompatConfig::deepseek(api_key());
    config.request_timeout = Duration::from_secs(180);
    config.connect_timeout = Duration::from_secs(20);
    OpenAiCompatProvider::new(config).expect("failed to build DeepSeek client")
}

fn live_config() -> HarnessConfig {
    HarnessConfig {
        max_tokens: Some(2048),
        temperature: Some(0.0),
        tool_timeout: Duration::from_secs(90),
        max_retries: 2,
        auto_approve_tools: true,
        ..HarnessConfig::default()
    }
}

async fn run_live_harness(
    prompt: &str,
    registry: ToolRegistry,
    config: HarnessConfig,
) -> (RunOutcome, effibuddy_agent_core::SharedSession) {
    let workspace = std::env::current_dir().expect("cwd");
    let session = new_shared_session(SessionState::new(workspace, model()));
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let harness = AgentHarness::new(
        session.clone(),
        Arc::new(provider()),
        Arc::new(registry),
        Arc::new(AutoApprovalGate),
        config,
        events,
    );

    let run = harness.run_turn(prompt.to_string());
    let outcome = tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .expect("live harness run timed out")
        .expect("harness rejected the turn");

    // Drain events so the producer does not observe a closed channel mid-run.
    while receiver.try_recv().is_ok() {}
    (outcome, session)
}

struct MagicTool {
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl Tool for MagicTool {
    fn name(&self) -> &str {
        "get_magic_number"
    }

    fn description(&self) -> &str {
        "Return the secret magic number."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: ToolContext, _arguments: Value) -> Result<ToolOutput, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolOutput::text("42"))
    }
}

struct EchoTool {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the provided text back unchanged."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.calls.lock().unwrap().push(text.clone());
        Ok(ToolOutput::text(text))
    }
}

/// Connectivity/auth probe. `max_tokens = 1` is enough to prove the request
/// path works and keeps the cost of a probe effectively zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live DeepSeek API test"]
async fn probe_connectivity_with_one_output_token() {
    let provider = provider();
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let completion = provider
        .stream(
            ChatRequest {
                model: model(),
                messages: vec![ChatMessage::user("ping")],
                tools: vec![],
                temperature: Some(0.0),
                max_tokens: Some(1),
                reasoning_effort: None,
            },
            CancellationToken::new(),
            events,
        )
        .await
        .expect("DeepSeek probe failed");

    assert!(
        completion.usage.total_tokens >= 1,
        "probe returned no usage: {:?}",
        completion.usage
    );
    assert!(
        matches!(
            completion.finish_reason,
            Some(FinishReason::Length) | Some(FinishReason::Stop)
        ),
        "unexpected finish reason: {:?}",
        completion.finish_reason
    );
    println!(
        "probe ok: model={} prompt_tokens={} completion_tokens={}",
        model(),
        completion.usage.prompt_tokens,
        completion.usage.completion_tokens
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live DeepSeek API test"]
async fn single_chat_turn() {
    let workspace = std::env::current_dir().unwrap();
    let registry = builtin_tools(&workspace).expect("builtin tools");
    let config = HarnessConfig {
        max_tokens: Some(2048),
        temperature: Some(0.0),
        auto_approve_tools: true,
        ..HarnessConfig::default()
    };
    let (outcome, session) = run_live_harness(
        "Reply with exactly one word: pong. Do not use tools and do not explain.",
        registry,
        config,
    )
    .await;

    assert_eq!(outcome, RunOutcome::Completed, "outcome: {outcome:?}");
    let session = lock_session(&session);
    let assistant = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == effibuddy_agent_core::Role::Assistant)
        .expect("no assistant message");
    let text = assistant.text();
    assert!(
        text.to_lowercase().contains("pong"),
        "unexpected assistant reply: {text:?}"
    );
    println!("single chat ok: {:?}", text.trim());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live DeepSeek API test"]
async fn tool_loop_executes_a_tool_and_answers() {
    let calls = Arc::new(Mutex::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(MagicTool {
        calls: calls.clone(),
    });

    let (outcome, session) = run_live_harness(
        "Call the get_magic_number tool exactly once, then answer with the number and nothing else.",
        registry,
        live_config(),
    )
    .await;

    assert_eq!(outcome, RunOutcome::Completed, "outcome: {outcome:?}");
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "tool was not called exactly once"
    );

    let session = lock_session(&session);
    let tool_result = session
        .messages
        .iter()
        .filter_map(|message| message.tool_result())
        .find(|result| result.name == "get_magic_number")
        .expect("no get_magic_number tool result");
    assert!(!tool_result.is_error, "{}", tool_result.content);
    assert!(tool_result.content.contains("42"));

    let final_text = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == effibuddy_agent_core::Role::Assistant)
        .map(|message| message.text())
        .unwrap_or_default();
    assert!(
        final_text.contains("42"),
        "final answer did not include the tool result: {final_text:?}"
    );
    println!("tool loop ok: {:?}", final_text.trim());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live DeepSeek API test"]
async fn ptc_loop_runs_javascript_that_calls_tools() {
    let echo_calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool {
        calls: echo_calls.clone(),
    });
    registry.register(RunJsTool::new());

    let (outcome, session) = run_live_harness(
        "Use the run_js tool exactly once. In the JavaScript, call tools.echo({text: \"alpha\"}) and \
         tools.echo({text: \"beta\"}), join the two results with a space, and return the joined string. \
         Then answer with that string and nothing else.",
        registry,
        live_config(),
    )
    .await;

    assert_eq!(outcome, RunOutcome::Completed, "outcome: {outcome:?}");
    assert_eq!(
        *echo_calls.lock().unwrap(),
        vec!["alpha".to_string(), "beta".to_string()],
        "echo should be called twice from inside the JS sandbox"
    );

    let session = lock_session(&session);
    let run_js_result = session
        .messages
        .iter()
        .filter_map(|message| message.tool_result())
        .find(|result| result.name == "run_js")
        .expect("model did not call run_js");
    assert!(!run_js_result.is_error, "{}", run_js_result.content);
    assert!(
        run_js_result.content.contains("alpha beta"),
        "unexpected JS result: {}",
        run_js_result.content
    );

    let final_text = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == effibuddy_agent_core::Role::Assistant)
        .map(|message| message.text())
        .unwrap_or_default();
    assert!(
        final_text.contains("alpha beta"),
        "final answer did not include the PTC result: {final_text:?}"
    );
    println!("ptc loop ok: {:?}", final_text.trim());
}
