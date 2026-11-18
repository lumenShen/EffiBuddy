//! Tool contract, approval gate and registry.
//!
//! Tools are capability boundaries. Every call passes through an approval
//! policy before execution; even an auto-approved tool is still an explicit
//! decision recorded by the harness.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::harness::HarnessEvent;
use crate::ids::new_id;
use crate::message::{TodoItem, ToolCall};
use crate::provider::ToolSpec;
use crate::subagent::SubagentSpawner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Run without asking. Use for read-only, sandboxed capabilities.
    Auto,
    /// Ask the user through the approval gate.
    Ask,
    /// Refuse to run at all. Useful for locked-down workspaces.
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool_name: String,
    pub call_id: String,
    pub summary: String,
    pub arguments: Value,
    /// Risk of *this* call, as declared by the tool. The permission matrix uses
    /// it to decide whether a device tool (computer / browser) is allowed in a
    /// read-only mode such as [`PermissionMode::Plan`]. Defaults to
    /// [`ToolRisk::Mutating`] so a tool that forgets to declare anything can
    /// never quietly gain read-only treatment.
    #[serde(default)]
    pub risk: ToolRisk,
}

impl ApprovalRequest {
    pub fn new(call: &ToolCall, summary: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            summary: summary.into(),
            arguments: call.arguments.clone(),
            risk: ToolRisk::Mutating,
        }
    }

    /// Attach the tool-declared risk of this call.
    pub fn with_risk(mut self, risk: ToolRisk) -> Self {
        self.risk = risk;
        self
    }
}

/// How dangerous one concrete tool call is, independent of the tool itself.
///
/// File and command tools are classified by [`ToolClass`] alone. Device tools
/// (`computer`, `browser`) expose both observation and control through a single
/// tool name, so they declare the risk of each action and the matrix treats
/// [`ToolClass::Device`] accordingly: read-only device calls stay available in
/// plan mode, mutating ones are denied exactly like edits and commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// Observation only: snapshots, screenshots, listings, reading a page.
    ReadOnly,
    /// Changes external state: clicks, key presses, navigation, uploads.
    #[default]
    Mutating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    ApproveForSession,
    Deny,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision;
}

/// Used by tests and headless runs.
pub struct AutoApprovalGate;

#[async_trait]
impl ApprovalGate for AutoApprovalGate {
    async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

/// Used when a workspace is in "read-only / no tools" mode.
pub struct DenyApprovalGate;

#[async_trait]
impl ApprovalGate for DenyApprovalGate {
    async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// Bridges the async harness to the UI event loop.
///
/// The harness sends [`HarnessEvent::ApprovalRequested`], then awaits a
/// one-shot decision. The UI calls [`BrokerApprovalGate::respond`] from the
/// GPUI thread. A timeout or a closed channel denies the call, never allows it.
pub struct BrokerApprovalGate {
    events: tokio::sync::mpsc::UnboundedSender<HarnessEvent>,
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    timeout: Duration,
}

impl BrokerApprovalGate {
    pub fn new(
        events: tokio::sync::mpsc::UnboundedSender<HarnessEvent>,
        timeout: Duration,
    ) -> Self {
        Self {
            events,
            pending: Mutex::new(HashMap::new()),
            timeout,
        }
    }

    pub fn respond(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        let sender = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
        match sender {
            Some(sender) => sender.send(decision).is_ok(),
            None => false,
        }
    }

    /// Deny every pending request. Called when a run is interrupted so the UI
    /// does not keep showing approval cards for a dead run.
    pub fn deny_all(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, sender) in pending {
            let _ = sender.send(ApprovalDecision::Deny);
        }
    }
}

#[async_trait]
impl ApprovalGate for BrokerApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request.id.clone(), sender);

        if self
            .events
            .send(HarnessEvent::ApprovalRequested {
                request: request.clone(),
            })
            .is_err()
        {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&request.id);
            return ApprovalDecision::Deny;
        }

        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(decision)) => decision,
            _ => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&request.id);
                ApprovalDecision::Deny
            }
        }
    }
}

/// Global permission mode, mirroring ZCode's Shift+Tab cycle.
///
/// The mode is stored per session, shown as a composer chip and cycled with
/// Shift+Tab. [`ModeApprovalGate`] turns the mode into approval decisions
/// without the harness knowing about it: the harness keeps asking the gate for
/// every `Ask`-policy tool, and the gate decides who to bother.
///
/// # The mode is the whole policy
///
/// A mode is the user talking to the agent, not a hint. [`PermissionMode::FullAccess`]
/// literally means "don't ask me": nothing below this layer — not the harness,
/// not a tool's `execute` — may add a second gate that the user cannot see or
/// switch off. Tools declare *capabilities* ([`Tool::approval`], [`Tool::risk`],
/// [`ToolClass`]); deciding whether the user is willing to spend them is this
/// enum's job alone.
///
/// The failure this rule prevents has happened: `webfetch` once refused
/// loopback/private URLs on its own, which made `FullAccess` a lie and could be
/// bypassed with `shell` anyway. The fix was to delete that gate, not to expose
/// it — an opt-in `WebFetchTool::public_only` remains for hosts that want a
/// wall, off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask before every edit and command (default).
    #[default]
    Default,
    /// Approve file edits automatically; commands still go to the inner gate.
    AcceptEdits,
    /// Read-only research. Edits and commands are denied up front so the model
    /// produces a plan instead of acting.
    Plan,
    /// Approve everything, including what [`ToolRisk::Mutating`] would otherwise
    /// deny. The user has said "don't ask me": tools must not second-guess it
    /// with a policy of their own (see the type-level note above).
    FullAccess,
}

impl PermissionMode {
    /// The next mode in the Shift+Tab cycle:
    /// default → accept_edits → plan → full_access → default.
    pub fn next(self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::FullAccess,
            PermissionMode::FullAccess => PermissionMode::Default,
        }
    }
}

/// Coarse capability class of a tool, keyed by tool name.
///
/// The classification is deliberately name-based so it works for every
/// registry without inspecting tool implementations: known read-only helpers,
/// known file editors, known command runners, known device controllers;
/// anything else falls into [`ToolClass::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Reading state: files, directories, search, todo lists.
    ReadOnly,
    /// Mutating files inside the workspace.
    Edit,
    /// Running programs: shell, PTC scripts, workflow runs.
    Command,
    /// Driving hardware outside the workspace: the user's screen and keyboard
    /// (`computer`) or a browser (`browser`). Device tools mix observation and
    /// control under one name, so their per-call [`ToolRisk`] decides.
    Device,
    /// Anything that is none of the above (agents, messaging, …).
    Other,
}

impl ToolClass {
    pub fn of_tool_name(name: &str) -> Self {
        match name {
            "read_file" | "list_dir" | "grep" | "todo_write" | "webfetch" => ToolClass::ReadOnly,
            "write_file" | "edit_file" => ToolClass::Edit,
            "shell" | "run_js" | "workflow_run" => ToolClass::Command,
            "computer" | "browser" => ToolClass::Device,
            _ => ToolClass::Other,
        }
    }
}

/// Wraps another [`ApprovalGate`] with the session's [`PermissionMode`].
///
/// Decision matrix:
///
/// | mode          | edit            | command         | device (read)   | device (mutate) | read-only       | other   |
/// |---------------|-----------------|-----------------|-----------------|-----------------|-----------------|---------|
/// | Default       | forward         | forward         | forward         | forward         | forward         | forward |
/// | AcceptEdits   | approve locally | forward         | forward         | forward         | forward         | forward |
/// | Plan          | deny locally    | deny locally    | approve locally | deny locally    | approve locally | forward |
/// | FullAccess    | approve locally | approve locally | approve locally | approve locally | approve locally | approve |
///
/// Locally decided calls never reach the inner gate, so the UI never shows an
/// approval card for them and `mode` is re-read on every request — switching
/// modes mid-run takes effect immediately. The harness consults this gate for
/// every tool while in Plan mode (not only `Ask`-policy tools), so Auto tools
/// such as `run_js` and `agent` are held to the read-only research contract.
///
/// Device tools (computer / browser) are *not* a separate approval layer: the
/// `Off` half of their preference removes them from the registry entirely, and
/// once registered they are gated by this matrix like every other capability.
/// FullAccess therefore reaches subagents and the main agent alike.
pub struct ModeApprovalGate {
    inner: Arc<dyn ApprovalGate>,
    mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
}

impl ModeApprovalGate {
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        Self { inner, mode }
    }
}

#[async_trait]
impl ApprovalGate for ModeApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
        let class = ToolClass::of_tool_name(&request.tool_name);
        match (self.mode)() {
            PermissionMode::Default => self.inner.request_approval(request).await,
            PermissionMode::FullAccess => ApprovalDecision::Approve,
            PermissionMode::AcceptEdits => match class {
                ToolClass::Edit => ApprovalDecision::Approve,
                _ => self.inner.request_approval(request).await,
            },
            PermissionMode::Plan => match class {
                ToolClass::Edit | ToolClass::Command => ApprovalDecision::Deny,
                ToolClass::ReadOnly => ApprovalDecision::Approve,
                // Observation stays available for research; anything that
                // changes the desktop or the page is a mutation.
                ToolClass::Device => match request.risk {
                    ToolRisk::ReadOnly => ApprovalDecision::Approve,
                    ToolRisk::Mutating => ApprovalDecision::Deny,
                },
                ToolClass::Other => self.inner.request_approval(request).await,
            },
        }
    }
}

/// Lets a tool invoke other registered tools through the harness.
///
/// This is the bridge that makes programmatic tool calling (PTC) possible:
/// a JS/Python script running inside one tool can call `read_file` or
/// `list_dir` and receive the same approval-checked, truncated observation a
/// direct model tool call would get.
#[async_trait]
pub trait NestedToolInvoker: Send + Sync {
    /// Names exposed to a script as `tools.<name>`.
    fn tool_names(&self) -> Vec<String>;

    async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<String, String>;
}

/// Everything a tool is allowed to touch.
#[derive(Clone)]
pub struct ToolContext {
    /// Sandbox root (the session cwd). Built-in tools reject paths that escape
    /// it; relative paths resolve against it.
    pub cwd: PathBuf,
    /// Additional project folders the agent may read and write. Absolute paths
    /// inside any of these roots are accepted; relative paths still anchor at
    /// [`ToolContext::cwd`].
    pub additional_workspaces: Vec<PathBuf>,
    pub session_id: String,
    /// Runtime identity of the calling agent (`main` or `agent_…`). Tools use
    /// this to sign inter-agent messages.
    pub agent_id: String,
    /// Model-facing handle of the calling agent, when it has one.
    pub agent_name: Option<String>,
    /// 0 for the main agent, 1 for its children, and so on.
    pub agent_depth: usize,
    pub call_id: String,
    pub cancel: CancellationToken,
    /// Present when the harness allows this tool to orchestrate other tools.
    pub nested: Option<Arc<dyn NestedToolInvoker>>,
    /// Present when the host supports launching subagents.
    pub subagents: Option<Arc<dyn SubagentSpawner>>,
    /// Present when the host wants file edits recorded for conversation rewind.
    pub checkpoint_sink: Option<crate::checkpoint::SharedFileCheckpointSink>,
}

impl ToolContext {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: String,
    pub effects: Vec<ToolEffect>,
    /// UI-facing structured data. The harness copies this onto the resulting
    /// [`crate::message::ToolResult`]; it is never sent to the model.
    pub metadata: Option<Value>,
    /// Model-visible rasters (screenshots). Kept out of `content` so output
    /// truncation never corrupts an image.
    pub images: Vec<crate::message::ImageAttachment>,
    /// Tools that completed but failed semantically (for example a subagent
    /// that returned a failed report) mark the observation as an error.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            effects: Vec::new(),
            metadata: None,
            images: Vec::new(),
            is_error: false,
        }
    }

    pub fn with_effect(mut self, effect: ToolEffect) -> Self {
        self.effects.push(effect);
        self
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_image(mut self, image: crate::message::ImageAttachment) -> Self {
        self.images.push(image);
        self
    }

    pub fn with_images(mut self, images: Vec<crate::message::ImageAttachment>) -> Self {
        self.images.extend(images);
        self
    }

    pub fn with_error(mut self) -> Self {
        self.is_error = true;
        self
    }
}

#[derive(Debug, Clone)]
pub enum ToolEffect {
    SetTodos(Vec<TodoItem>),
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;

    /// Whether this capability needs a consent card at all.
    ///
    /// This declares *what the tool needs*, never *who is allowed*: the session
    /// [`PermissionMode`] decides that, and it is the only layer that may.
    /// A tool must not bury its own policy inside `execute` (refusing targets a
    /// mode would allow, hiding an approval, capping a capability) — such a
    /// gate is invisible to the user, cannot be switched off, and makes
    /// [`PermissionMode::FullAccess`] a promise the app does not keep.
    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    /// Risk of one concrete call. Only consulted for [`ToolClass::Device`]
    /// tools, which mix observation and control under a single name; leaving
    /// the default is safe because [`ToolRisk::Mutating`] is the strict answer.
    fn risk(&self, _arguments: &Value) -> ToolRisk {
        ToolRisk::Mutating
    }

    /// Group key for "allow for the rest of this session".
    ///
    /// Approving a tool for the session is cached by `name` unless the tool
    /// asks for finer granularity. Tools that mix capabilities under one name
    /// (device tools) return the concrete action instead, so:
    ///
    /// - approving `browser eval` once does not blanket-approve navigation or
    ///   uploads, and
    /// - forty consecutive `eval` calls ask exactly once.
    fn approval_key(&self, _arguments: &Value) -> Option<String> {
        None
    }

    /// Human-readable one-liner shown in the approval card.
    fn summarize(&self, arguments: &Value) -> String {
        let compact = serde_json::to_string(arguments).unwrap_or_default();
        format!("Run `{}` with {}", self.name(), compact)
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: Tool>(&mut self, tool: T) -> &mut Self {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
        self
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// A copy that only contains tools for which `keep` returns true. Used to
    /// give subagents a narrower capability surface than their parent.
    pub fn filtered(&self, keep: impl Fn(&str) -> bool) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for (name, tool) in &self.tools {
            if keep(name) {
                registry.register_arc(tool.clone());
            }
        }
        registry
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo the input"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))
        }
    }

    #[test]
    fn registry_lists_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.specs()[0].name, "echo");
        assert!(registry.get("echo").is_some());
        assert!(registry.get("nope").is_none());
    }

    struct CountingGate {
        calls: std::sync::atomic::AtomicUsize,
        decision: ApprovalDecision,
    }

    impl CountingGate {
        fn new(decision: ApprovalDecision) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                decision,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ApprovalGate for CountingGate {
        async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.decision
        }
    }

    fn request(tool_name: &str) -> ApprovalRequest {
        ApprovalRequest::new(&ToolCall::new(tool_name, json!({})), "summary")
    }

    fn mode_gate(
        mode: PermissionMode,
        inner: Arc<CountingGate>,
    ) -> (ModeApprovalGate, Arc<CountingGate>) {
        let gate = ModeApprovalGate::new(inner.clone(), Arc::new(move || mode));
        (gate, inner)
    }

    #[test]
    fn permission_mode_cycles_in_order() {
        let mut mode = PermissionMode::default();
        assert_eq!(mode, PermissionMode::Default);
        mode = mode.next();
        assert_eq!(mode, PermissionMode::AcceptEdits);
        mode = mode.next();
        assert_eq!(mode, PermissionMode::Plan);
        mode = mode.next();
        assert_eq!(mode, PermissionMode::FullAccess);
        mode = mode.next();
        assert_eq!(mode, PermissionMode::Default);
    }

    #[test]
    fn tool_classification_covers_known_tools() {
        assert_eq!(ToolClass::of_tool_name("read_file"), ToolClass::ReadOnly);
        assert_eq!(ToolClass::of_tool_name("list_dir"), ToolClass::ReadOnly);
        assert_eq!(ToolClass::of_tool_name("grep"), ToolClass::ReadOnly);
        assert_eq!(ToolClass::of_tool_name("todo_write"), ToolClass::ReadOnly);
        assert_eq!(ToolClass::of_tool_name("webfetch"), ToolClass::ReadOnly);
        assert_eq!(ToolClass::of_tool_name("write_file"), ToolClass::Edit);
        assert_eq!(ToolClass::of_tool_name("edit_file"), ToolClass::Edit);
        assert_eq!(ToolClass::of_tool_name("shell"), ToolClass::Command);
        assert_eq!(ToolClass::of_tool_name("run_js"), ToolClass::Command);
        assert_eq!(ToolClass::of_tool_name("workflow_run"), ToolClass::Command);
        // Device tools are classified by name; their per-call risk decides the
        // read-only half of the matrix.
        assert_eq!(ToolClass::of_tool_name("computer"), ToolClass::Device);
        assert_eq!(ToolClass::of_tool_name("browser"), ToolClass::Device);
        assert_eq!(ToolClass::of_tool_name("agent"), ToolClass::Other);
    }

    fn device_request(tool: &str, risk: ToolRisk) -> ApprovalRequest {
        ApprovalRequest::new(&ToolCall::new(tool, json!({"action": "any"})), "summary")
            .with_risk(risk)
    }

    #[tokio::test]
    async fn plan_mode_keeps_device_observation_and_denies_device_control() {
        let (gate, inner) = mode_gate(
            PermissionMode::Plan,
            Arc::new(CountingGate::new(ApprovalDecision::Approve)),
        );
        for tool in ["computer", "browser"] {
            assert_eq!(
                gate.request_approval(device_request(tool, ToolRisk::Mutating))
                    .await,
                ApprovalDecision::Deny,
                "plan mode must deny mutating `{tool}`",
            );
            assert_eq!(
                gate.request_approval(device_request(tool, ToolRisk::ReadOnly))
                    .await,
                ApprovalDecision::Approve,
                "plan mode must allow observing `{tool}`",
            );
        }
        assert_eq!(
            inner.calls(),
            0,
            "plan mode decides device calls without a consent card"
        );
    }

    #[tokio::test]
    async fn full_access_approves_device_tools_without_a_card() {
        // Regression for the old consent layer: FullAccess used to be stopped by
        // a computer/browser consent card that sat outside this matrix.
        let (gate, inner) = mode_gate(
            PermissionMode::FullAccess,
            Arc::new(CountingGate::new(ApprovalDecision::Deny)),
        );
        for tool in ["computer", "browser"] {
            for risk in [ToolRisk::ReadOnly, ToolRisk::Mutating] {
                assert_eq!(
                    gate.request_approval(device_request(tool, risk)).await,
                    ApprovalDecision::Approve,
                    "full access must approve `{tool}` ({risk:?})",
                );
            }
        }
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn full_access_approves_webfetch_without_a_card() {
        // The tool must not add a policy layer of its own: with the user on
        // FullAccess a fetch goes straight through, local targets included.
        let (gate, inner) = mode_gate(
            PermissionMode::FullAccess,
            Arc::new(CountingGate::new(ApprovalDecision::Deny)),
        );
        assert_eq!(
            gate.request_approval(request("webfetch")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn default_mode_still_asks_before_webfetch() {
        // ...and in Default mode the URL shows up on a normal consent card.
        let (gate, inner) = mode_gate(
            PermissionMode::Default,
            Arc::new(CountingGate::new(ApprovalDecision::ApproveForSession)),
        );
        assert_eq!(
            gate.request_approval(request("webfetch")).await,
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn default_and_accept_edits_still_ask_for_device_control() {
        for mode in [PermissionMode::Default, PermissionMode::AcceptEdits] {
            let (gate, inner) = mode_gate(
                mode,
                Arc::new(CountingGate::new(ApprovalDecision::ApproveForSession)),
            );
            assert_eq!(
                gate.request_approval(device_request("computer", ToolRisk::Mutating))
                    .await,
                ApprovalDecision::ApproveForSession,
            );
            assert_eq!(inner.calls(), 1, "{mode:?} must show a real card");
        }
    }

    #[tokio::test]
    async fn plan_mode_denies_edits_and_commands_without_touching_inner_gate() {
        let (gate, inner) = mode_gate(
            PermissionMode::Plan,
            Arc::new(CountingGate::new(ApprovalDecision::Approve)),
        );
        for tool in ["write_file", "edit_file", "shell", "run_js", "workflow_run"] {
            assert_eq!(
                gate.request_approval(request(tool)).await,
                ApprovalDecision::Deny,
                "plan mode must deny `{tool}`",
            );
        }
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn plan_mode_approves_read_only_tools_locally_and_forwards_other() {
        let (gate, inner) = mode_gate(
            PermissionMode::Plan,
            Arc::new(CountingGate::new(ApprovalDecision::Approve)),
        );
        // Read-only research runs without prompting in Plan mode.
        for tool in ["read_file", "list_dir", "grep", "todo_write", "webfetch"] {
            assert_eq!(
                gate.request_approval(request(tool)).await,
                ApprovalDecision::Approve,
                "plan mode must allow read-only `{tool}`",
            );
        }
        assert_eq!(inner.calls(), 0);
        // `Other`-class tools (agents, messaging) still reach the inner gate so
        // the user decides whether Plan mode may spawn or contact an agent.
        assert_eq!(
            gate.request_approval(request("agent")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn accept_edits_auto_approves_edits_and_forwards_commands() {
        let (gate, inner) = mode_gate(
            PermissionMode::AcceptEdits,
            Arc::new(CountingGate::new(ApprovalDecision::Deny)),
        );
        assert_eq!(
            gate.request_approval(request("write_file")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(
            gate.request_approval(request("edit_file")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(inner.calls(), 0);
        // Commands still ask the inner gate, even when it denies.
        assert_eq!(
            gate.request_approval(request("shell")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn full_access_approves_everything_without_touching_inner_gate() {
        let (gate, inner) = mode_gate(
            PermissionMode::FullAccess,
            Arc::new(CountingGate::new(ApprovalDecision::Deny)),
        );
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "shell",
            "run_js",
            "workflow_run",
        ] {
            assert_eq!(
                gate.request_approval(request(tool)).await,
                ApprovalDecision::Approve,
                "full access must approve `{tool}`",
            );
        }
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn default_mode_forwards_everything_to_inner_gate() {
        let (gate, inner) = mode_gate(
            PermissionMode::Default,
            Arc::new(CountingGate::new(ApprovalDecision::ApproveForSession)),
        );
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "shell",
            "run_js",
            "workflow_run",
        ] {
            assert_eq!(
                gate.request_approval(request(tool)).await,
                ApprovalDecision::ApproveForSession,
                "default mode must forward `{tool}`",
            );
        }
        assert_eq!(inner.calls(), 6);
    }

    #[tokio::test]
    async fn mode_is_read_fresh_on_every_request() {
        let inner = Arc::new(CountingGate::new(ApprovalDecision::Deny));
        let current = Arc::new(std::sync::atomic::AtomicU8::new(
            PermissionMode::Default as u8,
        ));
        let mode = current.clone();
        let gate = ModeApprovalGate::new(
            inner.clone(),
            Arc::new(
                move || match mode.load(std::sync::atomic::Ordering::SeqCst) {
                    1 => PermissionMode::AcceptEdits,
                    2 => PermissionMode::Plan,
                    _ => PermissionMode::Default,
                },
            ),
        );
        assert_eq!(
            gate.request_approval(request("shell")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(inner.calls(), 1);
        current.store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            gate.request_approval(request("shell")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(inner.calls(), 1, "plan mode denies locally, no inner call");
    }
}
