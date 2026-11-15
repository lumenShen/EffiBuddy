//! Built-in tools for EffiBuddy.
//!
//! The registry is intentionally small and composable:
//!
//! - file tools (`read_file`, `write_file`, `edit_file`, `list_dir`)
//! - workspace search (`grep`)
//! - process execution (`shell`)
//! - plan tracking (`todo_write`)
//! - programmatic tool calling (`run_js`), backed by the Boa sandbox in
//!   `effibuddy-agent-js`
//!
//! All path-based tools resolve through [`Workspace`], which rejects paths
//! outside the workspace root. Mutating tools require approval.

pub mod agent;
pub mod args;
pub mod computer;
mod files;
mod grep;
pub mod messaging;
mod ptc;
mod shell;
mod todo;
pub mod workflow;
pub mod workspace;

// Stress lab (opt-in via the `stress` feature).
#[cfg(feature = "stress")]
pub mod stress_tools;

pub use agent::AgentTool;
pub use computer::ComputerTool;
pub use files::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use grep::GrepTool;
pub use messaging::{AgentListTool, AgentSendTool};
pub use ptc::RunJsTool;
pub use shell::ShellTool;
pub use todo::TodoWriteTool;
pub use workflow::WorkflowTool;
pub use workspace::Workspace;

#[cfg(feature = "stress")]
pub use stress_tools::{VirtualWorkspace, stress_real_tools, stress_tools};

use std::path::PathBuf;

use effibuddy_agent_core::{ToolError, ToolRegistry};

/// Build the default tool set for a workspace.
pub fn builtin_tools(root: impl Into<PathBuf>) -> Result<ToolRegistry, ToolError> {
    let workspace = Workspace::new(root)?;
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(workspace.clone()));
    registry.register(WriteFileTool::new(workspace.clone()));
    registry.register(EditFileTool::new(workspace.clone()));
    registry.register(ListDirTool::new(workspace.clone()));
    registry.register(GrepTool::new(workspace.clone()));
    registry.register(ShellTool::new(workspace));
    registry.register(TodoWriteTool::new());
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
        AgentHarness, AutoApprovalGate, Completion, FileCheckpointSink, HarnessConfig,
        NestedToolInvoker, SessionState, Tool, ToolCall, ToolContext, ToolOutput, ToolRegistry,
        new_shared_session,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn context(nested: Option<Arc<dyn NestedToolInvoker>>) -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
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
}
