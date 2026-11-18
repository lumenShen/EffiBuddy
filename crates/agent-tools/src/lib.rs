//! Built-in tools for EffiBuddy.
//!
//! The registry is intentionally small and composable:
//!
//! - file tools (`read_file`, `write_file`, `edit_file`, `list_dir`)
//! - workspace search (`grep`)
//! - reading one public web page (`webfetch`)
//! - process execution (`shell`)
//! - plan tracking (`todo_write`)
//! - programmatic tool calling (`run_js`), backed by the Boa sandbox in
//!   `effibuddy-agent-js`
//!
//! All path-based tools resolve through [`Workspace`], which rejects paths
//! outside the workspace root. Mutating tools require approval.

// The `browser` tool's JSON schema (`json!`) expands one macro level per
// property; the default limit is no longer enough now that it documents every
// action and parameter.
#![recursion_limit = "512"]

pub mod agent;
pub mod args;
pub mod browser;
pub mod computer;
mod files;
mod grep;
pub mod messaging;
mod ptc;
mod shell;
pub mod state;
mod todo;
pub mod webfetch;
pub mod workflow;
pub mod workspace;

// Stress lab (opt-in via the `stress` feature).
#[cfg(feature = "stress")]
pub mod stress_tools;

pub use agent::AgentTool;
pub use browser::BrowserTool;
pub use computer::ComputerTool;
pub use files::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use grep::GrepTool;
pub use messaging::{AgentListTool, AgentSendTool};
pub use ptc::RunJsTool;
pub use shell::{SHELL_ENV, ShellSpec, ShellTool};
pub use state::{SessionStateTool, forget_session as forget_session_state};
pub use todo::TodoWriteTool;
pub use webfetch::WebFetchTool;
pub use workflow::WorkflowTool;
pub use workspace::Workspace;

#[cfg(feature = "stress")]
pub use stress_tools::{VirtualWorkspace, stress_real_tools, stress_tools};

use std::path::PathBuf;

use effibuddy_agent_core::{ToolError, ToolRegistry};

/// Build the default tool set for a workspace.
pub fn builtin_tools(root: impl Into<PathBuf>) -> Result<ToolRegistry, ToolError> {
    builtin_tools_with_roots(root, &[])
}

/// Build the default tool set for a project with multiple folders: relative
/// paths resolve against `primary`, and paths inside any of `additional` are
/// readable and writable too.
pub fn builtin_tools_with_roots(
    primary: impl Into<PathBuf>,
    additional: &[PathBuf],
) -> Result<ToolRegistry, ToolError> {
    builtin_tools_with_vision(primary, additional, false)
}

/// Like [`builtin_tools_with_roots`], but `read_file` can also hand image files
/// back as model-visible image parts.
///
/// Hosts pass the selected model's vision capability: a model that cannot see
/// images must not receive them, so `read_file` describes an image instead of
/// attaching it.
pub fn builtin_tools_with_vision(
    primary: impl Into<PathBuf>,
    additional: &[PathBuf],
    vision: bool,
) -> Result<ToolRegistry, ToolError> {
    let workspace = Workspace::with_roots(primary, additional)?;
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(workspace.clone()).with_vision(vision));
    registry.register(WriteFileTool::new(workspace.clone()));
    registry.register(EditFileTool::new(workspace.clone()));
    registry.register(ListDirTool::new(workspace.clone()));
    registry.register(GrepTool::new(workspace.clone()));
    registry.register(WebFetchTool::new());
    registry.register(ShellTool::new(workspace));
    registry.register(TodoWriteTool::new());
    registry.register(SessionStateTool::new());
    registry.register(RunJsTool::new());
    registry.register(AgentTool::new());
    registry.register(AgentSendTool::new());
    registry.register(AgentListTool::new());
    registry.register(WorkflowTool::new());
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use effibuddy_agent_core::{
        AgentHarness, ApprovalDecision, ApprovalGate, ApprovalRequest, AutoApprovalGate,
        Completion, FileCheckpointSink, HarnessConfig, ModeApprovalGate, NestedToolInvoker,
        PermissionMode, SessionState, Tool, ToolCall, ToolContext, ToolOutput, ToolRegistry,
        new_shared_session,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn context(nested: Option<Arc<dyn NestedToolInvoker>>) -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            additional_workspaces: Vec::new(),
            session_id: "test-session".to_string(),
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            call_id: "call-1".to_string(),
            cancel: CancellationToken::new(),
            nested,
            subagents: None,
            checkpoint_sink: None,
        }
    }

    struct EchoTool {
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo text"
        }
        fn parameters(&self) -> Value {
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            })
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.lock().unwrap().push(arguments.clone());
            Ok(ToolOutput::text(
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))
        }
    }

    struct MockNested;

    #[async_trait]
    impl NestedToolInvoker for MockNested {
        fn tool_names(&self) -> Vec<String> {
            vec!["echo".to_string()]
        }
        async fn invoke(
            &self,
            name: &str,
            arguments: Value,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            if name != "echo" {
                return Err(format!("unknown tool {name}"));
            }
            Ok(format!(
                "echo:{}",
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ))
        }
    }

    #[tokio::test]
    async fn file_tools_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let registry = builtin_tools(directory.path()).unwrap();

        let write = registry.get("write_file").unwrap();
        write
            .execute(
                context(None),
                json!({"path": "notes/a.txt", "content": "hello\nworld\n"}),
            )
            .await
            .unwrap();

        let read = registry.get("read_file").unwrap();
        let output = read
            .execute(context(None), json!({"path": "notes/a.txt"}))
            .await
            .unwrap();
        assert!(output.content.contains("1 | hello"), "{}", output.content);
        assert!(output.content.contains("2 | world"), "{}", output.content);

        let edit = registry.get("edit_file").unwrap();
        edit.execute(
            context(None),
            json!({"path": "notes/a.txt", "old_string": "world", "new_string": "effi"}),
        )
        .await
        .unwrap();

        let grep = registry.get("grep").unwrap();
        let found = grep
            .execute(context(None), json!({"pattern": "effi", "path": "."}))
            .await
            .unwrap();
        assert!(found.content.contains("notes/a.txt:2"), "{}", found.content);

        let list = registry.get("list_dir").unwrap();
        let listing = list
            .execute(context(None), json!({"path": "notes"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&listing.content).unwrap();
        assert_eq!(parsed["entries"][0]["name"], "a.txt");

        // Path escapes are rejected before touching the filesystem.
        let escape = read
            .execute(
                context(None),
                json!({"path": "../outside-the-workspace.txt"}),
            )
            .await;
        assert!(escape.is_err());
    }

    /// A PNG signature plus an `IHDR` chunk is enough for `read_file` to treat
    /// the file as an image; the tool never decodes pixels.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[tokio::test]
    async fn read_file_attaches_image_parts_for_vision_models() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("shot.png"), png_header(1920, 1080)).unwrap();

        let registry = builtin_tools_with_vision(directory.path(), &[], true).unwrap();
        let output = registry
            .get("read_file")
            .unwrap()
            .execute(context(None), json!({"path": "shot.png"}))
            .await
            .unwrap();
        assert_eq!(output.images.len(), 1, "{}", output.content);
        assert_eq!(output.images[0].mime_type, "image/png");
        assert_eq!(
            (output.images[0].width, output.images[0].height),
            (1920, 1080)
        );
        assert!(output.content.contains("1920x1080"), "{}", output.content);
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&output.images[0].data)
            .unwrap();
        assert_eq!(decoded, png_header(1920, 1080));
    }

    #[tokio::test]
    async fn read_file_describes_images_for_text_only_models() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("shot.png"), png_header(640, 480)).unwrap();

        // `builtin_tools` stays text-only: no image part reaches the provider…
        let registry = builtin_tools(directory.path()).unwrap();
        let output = registry
            .get("read_file")
            .unwrap()
            .execute(context(None), json!({"path": "shot.png"}))
            .await
            .unwrap();
        assert!(output.images.is_empty());
        assert!(
            output.content.contains("cannot see images"),
            "{}",
            output.content
        );
        assert!(output.content.contains("640x480"), "{}", output.content);
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn run_js_can_orchestrate_tools() {
        let tool = RunJsTool::new();
        let output = tool
            .execute(
                context(Some(Arc::new(MockNested))),
                json!({
                    "code": r#"
                        const first = tools.echo({ text: "one" });
                        const second = tools.echo({ text: "two" });
                        return { joined: first + "+" + second };
                    "#,
                }),
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(parsed["value"]["joined"], "echo:one+echo:two");
        assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn harness_runs_a_ptc_tool_call_end_to_end() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            calls: calls.clone(),
        });
        registry.register(RunJsTool::new());

        let session = new_shared_session(SessionState::new(std::env::temp_dir(), "test-model"));
        let provider = Arc::new(effibuddy_agent_core::testing::ScriptedProvider::new(vec![
            Completion::default().with_tool_call(ToolCall::new(
                "run_js",
                json!({
                    "code": r#"
                        const value = tools.echo({ text: "ptc" });
                        return { value, upper: value.toUpperCase() };
                    "#
                }),
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

        let outcome = harness.run_turn("use ptc").await.unwrap();
        assert_eq!(outcome, effibuddy_agent_core::RunOutcome::Completed);
        assert_eq!(&*calls.lock().unwrap(), &[json!({"text": "ptc"})]);

        let session = effibuddy_agent_core::lock_session(&session);
        assert_eq!(session.messages.len(), 4);
        let result = session.messages[2].tool_result().unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(
            // Observations are compact JSON; no space after the colon.
            result.content.contains("\"upper\":\"PTC\""),
            "{}",
            result.content
        );
        assert_eq!(session.messages[3].text(), "done");
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<String>>,
    }

    impl FileCheckpointSink for RecordingSink {
        fn before_write(&self, relative_path: &str, _absolute: &std::path::Path, tool: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("before {tool} {relative_path}"));
        }

        fn after_write(
            &self,
            relative_path: &str,
            _absolute: &std::path::Path,
            tool: &str,
            content: &[u8],
        ) {
            self.events
                .lock()
                .unwrap()
                .push(format!("after {tool} {relative_path} {}", content.len()));
        }

        fn write_failed(&self, relative_path: &str, _absolute: &std::path::Path, tool: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("failed {tool} {relative_path}"));
        }

        fn note_shell(&self, command: &str) {
            self.events.lock().unwrap().push(format!("shell {command}"));
        }
    }

    #[tokio::test]
    async fn file_tools_report_checkpoint_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let registry = builtin_tools(directory.path()).unwrap();
        let sink = Arc::new(RecordingSink::default());
        let mut ctx = context(None);
        ctx.checkpoint_sink = Some(sink.clone());

        registry
            .get("write_file")
            .unwrap()
            .execute(
                ctx.clone(),
                json!({"path": "notes/a.txt", "content": "hello"}),
            )
            .await
            .unwrap();
        registry
            .get("edit_file")
            .unwrap()
            .execute(
                ctx.clone(),
                json!({"path": "notes/a.txt", "old_string": "hello", "new_string": "bye"}),
            )
            .await
            .unwrap();
        registry
            .get("shell")
            .unwrap()
            .execute(ctx, json!({"command": "echo checkpoint"}))
            .await
            .unwrap();

        let events = sink.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "before write_file notes/a.txt".to_string(),
                "after write_file notes/a.txt 5".to_string(),
                "before edit_file notes/a.txt".to_string(),
                "after edit_file notes/a.txt 3".to_string(),
                "shell echo checkpoint".to_string(),
            ]
        );
    }

    /// Counts how often a call reached the *real* gate, i.e. how often the user
    /// would have been asked.
    #[derive(Default)]
    struct CountingGate {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingGate {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ApprovalGate for CountingGate {
        async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision::Deny
        }
    }

    /// FullAccess means "don't ask me" — for every tool we ship, not just the
    /// ones someone remembered to test.
    ///
    /// This is the regression guard for a whole class of bug: a tool that adds
    /// its own policy layer (an invisible blocklist, a hidden consent step)
    /// cannot be caught here, but any tool that starts routing around the
    /// matrix, or a new builtin that quietly reintroduces a card, will.
    #[tokio::test]
    async fn full_access_approves_every_builtin_tool_without_a_card() {
        let directory = tempfile::tempdir().unwrap();
        let registry = builtin_tools(directory.path()).unwrap();
        assert!(
            registry.len() >= 10,
            "expected the full builtin set, got {:?}",
            registry.names()
        );

        let inner = Arc::new(CountingGate::default());
        let gate = ModeApprovalGate::new(inner.clone(), Arc::new(|| PermissionMode::FullAccess));
        for name in registry.names() {
            let call = ToolCall::new(name.clone(), json!({}));
            let request = ApprovalRequest::new(&call, format!("run {name}"));
            assert_eq!(
                gate.request_approval(request).await,
                ApprovalDecision::Approve,
                "full access must not card `{name}`"
            );
        }
        assert_eq!(
            inner.calls(),
            0,
            "full access must never reach the real approval gate"
        );
    }
}
