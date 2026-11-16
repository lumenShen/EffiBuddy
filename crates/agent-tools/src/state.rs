//! Session-scoped key-value scratchpad (`state` tool).
//!
//! A long task keeps discovering facts that must survive the context window:
//! "the answer to question 3 is B", "step 7 maps to file `myArray.h`", "the
//! build output is in `/tmp/build.log`". Re-deriving them every turn is
//! expensive and lossy, so the model gets a tiny per-conversation map instead:
//!
//! - keys are strings, values are arbitrary JSON (already-parsed answers,
//!   arrays, objects);
//! - the store lives in process memory, scoped by session id, and is shared by
//!   the main agent and every tool call of that conversation;
//! - bounds keep it from becoming a context dumping ground: 200 keys per
//!   session and 16 000 characters per value.
//!
//! It has no external effect, so it is auto-approved: it is a scratchpad, not a
//! capability.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use effibuddy_agent_core::{ApprovalPolicy, Tool, ToolContext, ToolError, ToolOutput, ToolRisk};

use crate::args::required_string;

/// Hard cap on keys per session.
pub const MAX_KEYS: usize = 200;

/// Hard cap on one stored value, measured as rendered JSON characters.
pub const MAX_VALUE_CHARS: usize = 16_000;

/// The process-wide store: session id → key → value.
fn store() -> &'static Mutex<HashMap<String, BTreeMap<String, Value>>> {
    static STORE: OnceLock<Mutex<HashMap<String, BTreeMap<String, Value>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_store() -> std::sync::MutexGuard<'static, HashMap<String, BTreeMap<String, Value>>> {
    store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forget everything one conversation stored (called when it is deleted).
pub fn forget_session(session_id: &str) {
    lock_store().remove(session_id);
}

pub struct SessionStateTool;

impl SessionStateTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionStateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SessionStateTool {
    fn name(&self) -> &str {
        "state"
    }

    fn description(&self) -> &str {
        "Session-scoped key-value scratchpad (the `state_set` / `state_get` pair): store facts you \
         will need later — confirmed answers, mappings from step names to files, URLs, decisions — \
         instead of re-deriving them or carrying them in your context.\n\
         \n\
         - `action=set` with `key` and `value` writes or overwrites one entry.\n\
         - `action=get` with `key` reads one entry (missing keys are reported, not invented).\n\
         - `action=list` returns the keys and their sizes; `action=delete` removes one key.\n\
         - Values are arbitrary JSON. The store is scoped to this conversation, is shared with \
           `run_js` (`tools.state({action: \"get\", key: \"...\"})`), and does not survive the app."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "get", "list", "delete", "clear"]
                },
                "key": {"type": "string", "description": "entry name, e.g. \"answer.3\""},
                "value": {
                    "description": "any JSON value for action=set (string, number, array, object)"
                }
            },
            "required": ["action"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    fn risk(&self, arguments: &Value) -> ToolRisk {
        match arguments.get("action").and_then(Value::as_str) {
            Some("get" | "list") => ToolRisk::ReadOnly,
            _ => ToolRisk::Mutating,
        }
    }

    fn summarize(&self, arguments: &Value) -> String {
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("action");
        let key = arguments
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match action {
            "set" => format!("记录会话状态 {key}"),
            "get" => format!("读取会话状态 {key}"),
            "delete" => format!("删除会话状态 {key}"),
            "clear" => "清空会话状态".to_string(),
            other => format!("会话状态 {other}"),
        }
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let action = required_string(&arguments, "action")?;
        let session = ctx.session_id.clone();

        match action.as_str() {
            "set" => {
                let key = required_string(&arguments, "key")?;
                let value = arguments
                    .get("value")
                    .cloned()
                    .ok_or_else(|| ToolError::new("state set needs `value`"))?;
                let rendered = serde_json::to_string(&value).unwrap_or_default();
                if rendered.chars().count() > MAX_VALUE_CHARS {
                    return Err(ToolError::new(format!(
                        "value is {} characters; keep entries under {MAX_VALUE_CHARS} \
                         (store a path or a summary instead of the full payload)",
                        rendered.chars().count()
                    )));
                }
                let mut store = lock_store();
                let entries = store.entry(session).or_default();
                if !entries.contains_key(&key) && entries.len() >= MAX_KEYS {
                    return Err(ToolError::new(format!(
                        "this conversation already holds {MAX_KEYS} keys; delete some first"
                    )));
                }
                entries.insert(key.clone(), value);
                Ok(ToolOutput::text(compact(json!({
                    "action": "set",
                    "key": key,
                    "bytes": rendered.chars().count(),
                    "keys": entries.len(),
                }))))
            }
            "get" => {
                let key = required_string(&arguments, "key")?;
                let store = lock_store();
                let value = store
                    .get(&session)
                    .and_then(|entries| entries.get(&key))
                    .cloned();
                match value {
                    Some(value) => Ok(ToolOutput::text(compact(json!({
                        "action": "get",
                        "key": key,
                        "found": true,
                        "value": value,
                    })))),
                    None => {
                        let available: Vec<&String> = store
                            .get(&session)
                            .map(|entries| entries.keys().collect())
                            .unwrap_or_default();
                        Ok(ToolOutput::text(compact(json!({
                            "action": "get",
                            "key": key,
                            "found": false,
                            "keys": available,
                            "hint": "use action=list to inspect the store; missing keys are never invented",
                        }))))
                    }
                }
            }
            "list" => {
                let store = lock_store();
                let entries = store.get(&session);
                let keys: Vec<Value> = entries
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|(key, value)| {
                                json!({
                                    "key": key,
                                    "chars": serde_json::to_string(value)
                                        .map(|text| text.chars().count())
                                        .unwrap_or(0),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(ToolOutput::text(compact(json!({
                    "action": "list",
                    "keys": keys,
                }))))
            }
            "delete" => {
                let key = required_string(&arguments, "key")?;
                let mut store = lock_store();
                let removed = store
                    .get_mut(&session)
                    .map(|entries| entries.remove(&key).is_some())
                    .unwrap_or(false);
                Ok(ToolOutput::text(compact(json!({
                    "action": "delete",
                    "key": key,
                    "removed": removed,
                }))))
            }
            "clear" => {
                let removed = lock_store()
                    .get_mut(&session)
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                lock_store().remove(&session);
                Ok(ToolOutput::text(compact(json!({
                    "action": "clear",
                    "removed": removed,
                }))))
            }
            other => Err(ToolError::new(format!(
                "unknown state action `{other}`; use set|get|list|delete|clear"
            ))),
        }
    }
}

fn compact(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn context(session: &str) -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            additional_workspaces: Vec::new(),
            session_id: session.to_string(),
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            call_id: "c".to_string(),
            cancel: CancellationToken::new(),
            nested: None,
            subagents: None,
            checkpoint_sink: None,
        }
    }

    async fn run(session: &str, arguments: Value) -> Value {
        let output = SessionStateTool::new()
            .execute(context(session), arguments)
            .await
            .unwrap();
        serde_json::from_str(&output.content).unwrap()
    }

    #[tokio::test]
    async fn values_round_trip_and_survive_between_calls() {
        forget_session("state-test-roundtrip");
        run(
            "state-test-roundtrip",
            json!({"action": "set", "key": "answer.3", "value": {"choice": "B", "why": "build"}}),
        )
        .await;
        let value = run(
            "state-test-roundtrip",
            json!({"action": "get", "key": "answer.3"}),
        )
        .await;
        assert_eq!(value["found"], true);
        assert_eq!(value["value"]["choice"], "B");

        let listed = run("state-test-roundtrip", json!({"action": "list"})).await;
        assert_eq!(listed["keys"][0]["key"], "answer.3");
        forget_session("state-test-roundtrip");
    }

    #[tokio::test]
    async fn a_missing_key_reports_the_keys_that_do_exist() {
        forget_session("state-test-missing");
        run(
            "state-test-missing",
            json!({"action": "set", "key": "known", "value": 1}),
        )
        .await;
        let value = run(
            "state-test-missing",
            json!({"action": "get", "key": "unknown"}),
        )
        .await;
        assert_eq!(value["found"], false);
        assert_eq!(value["keys"][0], "known");
        forget_session("state-test-missing");
    }

    #[tokio::test]
    async fn sessions_do_not_see_each_other() {
        forget_session("state-test-a");
        forget_session("state-test-b");
        run(
            "state-test-a",
            json!({"action": "set", "key": "k", "value": 1}),
        )
        .await;
        let other = run("state-test-b", json!({"action": "get", "key": "k"})).await;
        assert_eq!(other["found"], false);
        forget_session("state-test-a");
        forget_session("state-test-b");
    }

    #[tokio::test]
    async fn oversized_values_are_refused_with_advice() {
        forget_session("state-test-big");
        let error = SessionStateTool::new()
            .execute(
                context("state-test-big"),
                json!({"action": "set", "key": "big", "value": "x".repeat(MAX_VALUE_CHARS + 1)}),
            )
            .await
            .expect_err("must fail");
        assert!(error.to_string().contains("keep entries under"), "{error}");
        forget_session("state-test-big");
    }

    #[test]
    fn reads_are_read_only_and_writes_mutate() {
        let tool = SessionStateTool::new();
        assert_eq!(
            tool.risk(&json!({"action": "get", "key": "a"})),
            ToolRisk::ReadOnly
        );
        assert_eq!(
            tool.risk(&json!({"action": "set", "key": "a", "value": 1})),
            ToolRisk::Mutating
        );
        assert_eq!(tool.approval(), ApprovalPolicy::Auto);
    }
}
