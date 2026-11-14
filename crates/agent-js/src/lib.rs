//! A small, pure-Rust JavaScript sandbox for **programmatic tool calling**
//! (PTC).
//!
//! Instead of asking the model for one tool call at a time, PTC lets it write a
//! short program that orchestrates tools:
//!
//! ```js
//! const files = tools.list_dir({ path: "." });
//! const manifests = files
//!   .filter((entry) => entry.name.endsWith("Cargo.toml"))
//!   .map((entry) => ({ path: entry.path, body: tools.read_file({ path: entry.path }).slice(0, 400) }));
//! return manifests;
//! ```
//!
//! The interpreter is [Boa](https://github.com/boa-dev/boa), a JavaScript
//! engine written entirely in Rust. Boa is embedded in-process, while tool
//! calls are proxied back to the async host through a blocking bridge: the JS
//! thread sends a request and waits, the async side runs the real tool (with
//! approvals, timeouts and sandboxing), then resolves the call.
//!
//! # Safety model
//!
//! This is a *tool orchestration* sandbox, not a security boundary for hostile
//! code:
//!
//! - no filesystem, network, process or environment APIs are injected;
//! - scripts can only reach the tools explicitly offered by the host;
//! - loop iterations, recursion, stack size, script size, number of tool calls
//!   and output/log sizes are capped;
//! - wall-clock and cancellation are enforced by the host, and tool calls have
//!   their own timeout.
//!
//! Boa has no preemptive interrupt or hard memory cap, so a pathological script
//! can still consume CPU until it hits the loop limit. Run untrusted code in a
//! child process if that matters for your threat model.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use boa_engine::{
    Context, JsError, JsNativeError, JsResult, JsValue, NativeFunction, Source, js_str, js_string,
    object::FunctionObjectBuilder, property::Attribute, vm::RuntimeLimits,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Wall-clock budget for the whole script, including tool calls.
    pub timeout: Duration,
    /// How long the JS thread waits for one tool result.
    pub tool_wait_timeout: Duration,
    pub max_code_chars: usize,
    pub max_tool_calls: usize,
    pub max_logs: usize,
    pub max_log_chars: usize,
    pub max_output_chars: usize,
    pub max_loop_iterations: u64,
    pub max_stack_size: usize,
    pub max_recursion: usize,
    /// JavaScript appended before the user's code. Used by higher-level tools
    /// to inject a small DSL (for example the `workflow` builder) without
    /// exposing extra host capabilities.
    pub prelude: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            tool_wait_timeout: Duration::from_secs(120),
            max_code_chars: 24_000,
            max_tool_calls: 32,
            max_logs: 100,
            max_log_chars: 2_000,
            max_output_chars: 24_000,
            max_loop_iterations: 2_000_000,
            max_stack_size: 10 * 1024,
            max_recursion: 256,
            prelude: String::new(),
        }
    }
}

/// Executes one tool call on behalf of the script.
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    /// Return the tool's textual observation, or a human-readable error.
    /// Implementations must apply their own approval policy before doing work.
    async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<String, String>;
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: Value,
    pub ok: bool,
    pub content: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ScriptResult {
    /// The script's return value converted to JSON. `undefined` becomes `null`.
    pub value: Value,
    pub logs: Vec<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Error)]
pub enum SandboxError {
    #[error("script is too large ({size} characters, limit {limit})")]
    CodeTooLarge { size: usize, limit: usize },
    #[error("JavaScript error: {0}")]
    Script(String),
    #[error("tool bridge error: {0}")]
    Bridge(String),
    #[error("script timed out after {0:?}")]
    Timeout(Duration),
    #[error("script was cancelled")]
    Cancelled,
    #[error("sandbox task failed: {0}")]
    Internal(String),
}

enum ToolResponse {
    Ok(String),
    Err(String),
}

struct ToolRequest {
    name: String,
    arguments: Value,
    response: std::sync::mpsc::Sender<ToolResponse>,
}

struct Bridge {
    sender: UnboundedSender<ToolRequest>,
    remaining_calls: usize,
    response_timeout: Duration,
}

thread_local! {
    static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
    static LOGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static LOG_LIMITS: RefCell<(usize, usize)> = const { RefCell::new((100, 2_000)) };
}

struct BridgeGuard;

impl Drop for BridgeGuard {
    fn drop(&mut self) {
        BRIDGE.with(|slot| {
            slot.borrow_mut().take();
        });
        LOGS.with(|logs| logs.borrow_mut().clear());
    }
}

pub struct JsSandbox {
    config: SandboxConfig,
}

impl JsSandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Run a script that may call `tools.<name>(args)` and return a JSON value.
    pub async fn run(
        &self,
        code: impl Into<String>,
        tool_names: Vec<String>,
        invoker: Arc<dyn ToolInvoker>,
        cancel: CancellationToken,
    ) -> Result<ScriptResult, SandboxError> {
        let code = code.into();
        let code_size = code.chars().count();
        if code_size > self.config.max_code_chars {
            return Err(SandboxError::CodeTooLarge {
                size: code_size,
                limit: self.config.max_code_chars,
            });
        }

        let started = Instant::now();
        let config = self.config.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let handle =
            tokio::task::spawn_blocking(move || run_sync(code, tool_names, sender, config));
        tokio::pin!(handle);

        let deadline = tokio::time::Instant::now() + self.config.timeout;
        let mut receiver_open = true;
        let mut records = Vec::new();

        let (value, logs) = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SandboxError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(SandboxError::Timeout(self.config.timeout));
                }
                request = receiver.recv(), if receiver_open => {
                    match request {
                        Some(request) => {
                            let call_started = Instant::now();
                            let name = request.name.clone();
                            let arguments = request.arguments.clone();
                            let result = tokio::select! {
                                _ = cancel.cancelled() => {
                                    let _ = request.response.send(ToolResponse::Err("script cancelled".to_string()));
                                    return Err(SandboxError::Cancelled);
                                }
                                _ = tokio::time::sleep_until(deadline) => {
                                    let _ = request.response.send(ToolResponse::Err("script timed out".to_string()));
                                    return Err(SandboxError::Timeout(self.config.timeout));
                                }
                                result = invoker.invoke(&request.name, request.arguments, &cancel) => result,
                            };
                            let duration_ms = call_started.elapsed().as_millis() as u64;
                            let (response, ok, content) = match result {
                                Ok(content) => (ToolResponse::Ok(content.clone()), true, content),
                                Err(error) => (ToolResponse::Err(error.clone()), false, error),
                            };
                            records.push(ToolCallRecord {
                                name,
                                arguments,
                                ok,
                                content: truncate_text(&content, self.config.max_log_chars),
                                duration_ms,
                            });
                            let _ = request.response.send(response);
                        }
                        None => receiver_open = false,
                    }
                }
                result = &mut handle => {
                    break result
                        .map_err(|error| SandboxError::Internal(error.to_string()))??;
                }
            }
        };

        Ok(ScriptResult {
            value,
            logs,
            tool_calls: records,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn run_sync(
    code: String,
    tool_names: Vec<String>,
    sender: UnboundedSender<ToolRequest>,
    config: SandboxConfig,
) -> Result<(Value, Vec<String>), SandboxError> {
    LOGS.with(|logs| logs.borrow_mut().clear());
    LOG_LIMITS.with(|limits| *limits.borrow_mut() = (config.max_logs, config.max_log_chars));
    BRIDGE.with(|slot| {
        *slot.borrow_mut() = Some(Bridge {
            sender,
            remaining_calls: config.max_tool_calls,
            response_timeout: config.tool_wait_timeout,
        });
    });
    let _guard = BridgeGuard;

    let mut context = Context::default();
    let mut limits: RuntimeLimits = context.runtime_limits();
    limits.set_loop_iteration_limit(config.max_loop_iterations);
    limits.set_stack_size_limit(config.max_stack_size);
    limits.set_recursion_limit(config.max_recursion);
    context.set_runtime_limits(limits);

    register_host_functions(&mut context).map_err(|error| {
        SandboxError::Internal(format!("failed to register host functions: {error}"))
    })?;

    let source = build_source(&code, &tool_names, &config.prelude);
    let value = context
        .eval(Source::from_bytes(source.as_bytes()))
        .map_err(|error| SandboxError::Script(format_js_error(&error)))?;

    if let Err(error) = context.run_jobs() {
        return Err(SandboxError::Script(format_js_error(&error)));
    }

    let json = value
        .to_json(&mut context)
        .map_err(|error| SandboxError::Script(format_js_error(&error)))?
        .unwrap_or(Value::Null);
    let json = truncate_json(json, config.max_output_chars);
    let logs = LOGS.with(|logs| logs.borrow().clone());
    Ok((json, logs))
}

fn register_host_functions(context: &mut Context) -> JsResult<()> {
    let call_tool = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(call_tool_native),
    )
    .name(js_str!("__effi_call_tool"))
    .length(2)
    .build();
    context.register_global_property(
        js_str!("__effi_call_tool"),
        call_tool,
        Attribute::default(),
    )?;

    let log = FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(log_native))
        .name(js_str!("__effi_log"))
        .length(1)
        .build();
    context.register_global_property(js_str!("__effi_log"), log, Attribute::default())?;

    Ok(())
}

fn call_tool_native(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(JsValue::as_string)
        .map(|value| value.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("tool name must be a string"))?;
    let arguments_json = args
        .get(1)
        .and_then(JsValue::as_string)
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_else(|| "{}".to_string());
    let arguments: Value = serde_json::from_str(&arguments_json).map_err(|error| {
        JsNativeError::typ().with_message(format!("tool arguments must be JSON: {error}"))
    })?;

    let content = BRIDGE.with(|slot| -> JsResult<String> {
        let mut slot = slot.borrow_mut();
        let bridge = slot.as_mut().ok_or_else(|| {
            JsNativeError::error().with_message("tool bridge is unavailable in this sandbox")
        })?;
        if bridge.remaining_calls == 0 {
            return Err(JsNativeError::error()
                .with_message("tool call budget exhausted")
                .into());
        }
        bridge.remaining_calls -= 1;

        let (response_sender, response_receiver) = std::sync::mpsc::channel();
        if bridge
            .sender
            .send(ToolRequest {
                name,
                arguments,
                response: response_sender,
            })
            .is_err()
        {
            return Err(JsNativeError::error()
                .with_message("tool host is no longer running")
                .into());
        }

        match response_receiver.recv_timeout(bridge.response_timeout) {
            Ok(ToolResponse::Ok(content)) => Ok(content),
            Ok(ToolResponse::Err(error)) => Err(JsNativeError::error().with_message(error).into()),
            Err(_) => Err(JsNativeError::error()
                .with_message("tool call timed out")
                .into()),
        }
    })?;

    let _ = context;
    Ok(JsValue::from(js_string!(content)))
}

fn log_native(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let message = args
        .first()
        .map(|value| value.to_string(context))
        .transpose()?
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_default();
    LOGS.with(|logs| {
        let (max_logs, max_chars) = LOG_LIMITS.with(|limits| *limits.borrow());
        let mut logs = logs.borrow_mut();
        if logs.len() < max_logs {
            logs.push(truncate_text(&message, max_chars));
        } else if logs.len() == max_logs {
            logs.push("[more logs omitted]".to_string());
        }
    });
    Ok(JsValue::undefined())
}

fn build_source(code: &str, tool_names: &[String], prelude: &str) -> String {
    let mut source = String::with_capacity(code.len() + prelude.len() + 1024);
    source.push_str(
        r#""use strict";
const __effi_format = (value) => {
  try {
    if (typeof value === "string") return value;
    const text = JSON.stringify(value);
    return text === undefined ? String(value) : text;
  } catch (_) {
    return String(value);
  }
};
const console = {
  log: (...args) => __effi_log(args.map(__effi_format).join(" ")),
  info: (...args) => __effi_log(args.map(__effi_format).join(" ")),
  warn: (...args) => __effi_log("[warn] " + args.map(__effi_format).join(" ")),
  error: (...args) => __effi_log("[error] " + args.map(__effi_format).join(" ")),
};
const tools = Object.create(null);
const __effi_parse = (text) => {
  try { return JSON.parse(text); } catch (_) { return text; }
};
"#,
    );
    for name in tool_names {
        let json_name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
        source.push_str(&format!(
            "tools[{json_name}] = (args = {{}}) => __effi_parse(__effi_call_tool({json_name}, JSON.stringify(args)));\n"
        ));
    }
    source.push_str("const callTool = (name, args = {}) => __effi_parse(__effi_call_tool(String(name), JSON.stringify(args)));\n");
    if !prelude.trim().is_empty() {
        source.push_str(prelude);
        source.push('\n');
    }
    source.push_str("(function () {\n");
    source.push_str(code);
    source.push_str("\n})()\n");
    source
}

fn format_js_error(error: &JsError) -> String {
    let text = error.to_string();
    if text.trim().is_empty() {
        "unknown JavaScript error".to_string()
    } else {
        text
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn truncate_json(value: Value, max_chars: usize) -> Value {
    let text = serde_json::to_string(&value).unwrap_or_default();
    if text.chars().count() <= max_chars {
        return value;
    }
    Value::String(format!(
        "{}\n…[output truncated to {} characters]",
        truncate_text(&text, max_chars),
        max_chars
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingInvoker {
        calls: Mutex<Vec<(String, Value)>>,
        behavior: Box<dyn Fn(&str, &Value) -> Result<String, String> + Send + Sync>,
    }

    impl RecordingInvoker {
        fn new(
            behavior: impl Fn(&str, &Value) -> Result<String, String> + Send + Sync + 'static,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                behavior: Box::new(behavior),
            }
        }
    }

    #[async_trait]
    impl ToolInvoker for RecordingInvoker {
        async fn invoke(
            &self,
            name: &str,
            arguments: Value,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            (self.behavior)(name, &arguments)
        }
    }

    fn sandbox() -> JsSandbox {
        JsSandbox::new(SandboxConfig::default())
    }

    #[tokio::test]
    async fn evaluates_and_returns_json() {
        let invoker = Arc::new(RecordingInvoker::new(|_, _| Ok("".to_string())));
        let result = sandbox()
            .run(
                "const values = [1, 2, 3]; return { sum: values.reduce((a, b) => a + b, 0) };",
                vec![],
                invoker,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!({ "sum": 6 }));
    }

    #[tokio::test]
    async fn bridges_tool_calls_both_ways() {
        let invoker = Arc::new(RecordingInvoker::new(|name, args| {
            Ok(format!("{name}:{}", args["text"].as_str().unwrap_or("")))
        }));
        let result = sandbox()
            .run(
                r#"
                const a = tools.echo({ text: "hello" });
                const b = tools.echo({ text: "world" });
                return { a, b, both: a + " " + b };
                "#,
                vec!["echo".to_string()],
                invoker.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            result.value,
            serde_json::json!({
                "a": "echo:hello",
                "b": "echo:world",
                "both": "echo:hello echo:world"
            })
        );
        let calls = invoker.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "echo");
        assert_eq!(calls[0].1, serde_json::json!({ "text": "hello" }));
    }

    #[tokio::test]
    async fn tool_errors_are_catchable_in_js() {
        let invoker = Arc::new(RecordingInvoker::new(|_, _| Err("boom".to_string())));
        let result = sandbox()
            .run(
                r#"
                try {
                  tools.danger({});
                  return "not reached";
                } catch (error) {
                  return "caught: " + error.message;
                }
                "#,
                vec!["danger".to_string()],
                invoker,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!("caught: boom"));
    }

    #[tokio::test]
    async fn infinite_loops_hit_the_iteration_limit() {
        let config = SandboxConfig {
            max_loop_iterations: 10_000,
            ..SandboxConfig::default()
        };
        let invoker = Arc::new(RecordingInvoker::new(|_, _| Ok("".to_string())));
        let error = JsSandbox::new(config)
            .run("while (true) {}", vec![], invoker, CancellationToken::new())
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("loop") || text.contains("iteration") || text.contains("limit"),
            "unexpected error: {text}"
        );
    }

    #[tokio::test]
    async fn console_output_is_captured() {
        let invoker = Arc::new(RecordingInvoker::new(|_, _| Ok("".to_string())));
        let result = sandbox()
            .run(
                "console.log('hello', { answer: 42 }); return null;",
                vec![],
                invoker,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.logs, vec!["hello {\"answer\":42}".to_string()]);
    }

    #[tokio::test]
    async fn cancellation_stops_the_run() {
        struct SlowInvoker;

        #[async_trait]
        impl ToolInvoker for SlowInvoker {
            async fn invoke(
                &self,
                _name: &str,
                _arguments: Value,
                _cancel: &CancellationToken,
            ) -> Result<String, String> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok("".to_string())
            }
        }

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let sandbox = JsSandbox::new(SandboxConfig {
            tool_wait_timeout: Duration::from_millis(100),
            ..SandboxConfig::default()
        });
        let run = sandbox.run(
            "tools.echo({}); return 1;",
            vec!["echo".to_string()],
            Arc::new(SlowInvoker),
            cancel,
        );
        let canceller = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        };
        let (result, ()) = tokio::join!(run, canceller);
        assert!(matches!(result, Err(SandboxError::Cancelled)), "{result:?}");
    }

    #[tokio::test]
    async fn too_large_scripts_are_rejected() {
        let config = SandboxConfig {
            max_code_chars: 8,
            ..SandboxConfig::default()
        };
        let invoker = Arc::new(RecordingInvoker::new(|_, _| Ok("".to_string())));
        let result = JsSandbox::new(config)
            .run(
                "return 1; // long",
                vec![],
                invoker,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(SandboxError::CodeTooLarge { .. })));
    }
}
