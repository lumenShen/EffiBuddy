//! The agent harness.
//!
//! Design notes (adapted from the way DeepSeek's agent harness separates the
//! model policy from the execution environment):
//!
//! 1. **The transcript is the state.** Every decision and observation is
//!    appended to a durable message log; the UI renders that log and the next
//!    request is built from it. There is no hidden scratchpad that diverges
//!    from what the user sees.
//! 2. **One step = one model call + zero or more tool calls.** A run continues
//!    until the model stops calling tools; there is no step cap, and a run can
//!    be interrupted between any two await points.
//! 3. **The harness owns tool execution.** Approvals, timeouts, panic
//!    isolation, output truncation and error-to-observation conversion happen
//!    here, not in tools. A tool can fail without crashing a run.
//! 4. **Reasoning is separated from visible text.** Reasoning deltas are
//!    streamed to the UI but never sent back to the provider.
//! 5. **Interruptions keep the transcript well-formed.** If a run stops
//!    mid-tool batch, synthetic cancelled results are appended for every call,
//!    so the next request is still a valid tool-call/result sequence.
//! 6. **Context is compacted at user-turn boundaries**, never in the middle of
//!    a tool group, and the rolling summary is sent as a system block.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::context;
use crate::error::{AgentError, ProviderError, ToolError};
use crate::ids::new_id;
use crate::message::{Message, TodoItem, ToolCall, ToolResult, Usage};
use crate::prompt::{PromptContext, system_prompt};
use crate::provider::{ChatRequest, Completion, ModelProvider, ProviderEvent};
use crate::session::{CompactionRecord, RunOutcome, RunStatus, SharedSession, lock_session};
use crate::subagent::{SubagentProfile, builtin_subagent_profiles};
use crate::tool::{
    ApprovalDecision, ApprovalGate, ApprovalPolicy, ApprovalRequest, PermissionMode, ToolContext,
    ToolEffect, ToolRegistry,
};

#[derive(Clone, Debug)]
pub enum HarnessEvent {
    RunStarted {
        run_id: String,
        session_id: String,
    },
    StepStarted {
        run_id: String,
        step: usize,
    },
    AssistantStarted {
        message_id: String,
    },
    TextDelta {
        message_id: String,
        delta: String,
    },
    ReasoningDelta {
        message_id: String,
        delta: String,
    },
    AssistantFinished {
        message_id: String,
        interrupted: bool,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    ToolCallStarted {
        call: ToolCall,
    },
    ToolCallFinished {
        result: ToolResult,
    },
    TodosUpdated {
        todos: Vec<TodoItem>,
    },
    ContextCompacted {
        record: CompactionRecord,
    },
    UsageUpdated {
        usage: Usage,
    },
    RunFinished {
        run_id: String,
        outcome: RunOutcome,
    },
    /// A user message steered into the running turn was appended to the
    /// transcript at a step boundary. `steer_id` matches the value returned by
    /// [`AgentHandle::steer`]; the host uses it to drop its pending entry.
    SteerDrained {
        steer_id: String,
        message_id: String,
    },
    /// A structural change happened that is not covered by a delta.
    SessionChanged,
}

#[derive(Clone)]
pub struct HarnessConfig {
    /// Fraction of the provider context window that triggers compaction.
    pub compact_threshold: f32,
    /// User turns kept verbatim after compaction.
    pub keep_recent_turns: usize,
    /// Cap for the rolling summary itself.
    pub max_summary_chars: usize,
    /// Cap for one tool result before it enters the transcript.
    pub max_tool_output_chars: usize,
    pub tool_timeout: Duration,
    pub max_retries: usize,
    pub retry_base_delay: Duration,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Reasoning effort requested from models that expose levels.
    pub reasoning_effort: Option<String>,
    pub system_prompt_extra: String,
    /// Complete system prompt for this harness. Subagents use this instead of
    /// the default EffiBuddy prompt so their role and tool surface stay tight.
    pub system_prompt_override: Option<String>,
    /// The subagent catalog rendered into the system prompt. Shared with the
    /// host so Markdown-defined profiles reach the next step without
    /// rebuilding the harness. Defaults to the built-in profiles.
    pub agent_catalog: Arc<std::sync::RwLock<Vec<SubagentProfile>>>,
    /// Skip Ask approvals. Destructive tools still respect an explicit Deny.
    pub auto_approve_tools: bool,
    /// Override the provider's declared context window (tests, small models).
    pub context_window_override: Option<usize>,
    /// Runtime identity copied into every [`ToolContext`]. The main harness
    /// uses `main`; child harnesses receive their generated agent id.
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub agent_depth: usize,
    /// Host callback for file rewind checkpoints. File tools report every
    /// mutation here before/after writing; the host stores the original bytes
    /// so a user message can later be edited or undone without Git.
    pub checkpoint_sink: Option<crate::checkpoint::SharedFileCheckpointSink>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            compact_threshold: 0.78,
            keep_recent_turns: 6,
            max_summary_chars: 12_000,
            max_tool_output_chars: 24_000,
            tool_timeout: Duration::from_secs(180),
            max_retries: 2,
            retry_base_delay: Duration::from_millis(600),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            system_prompt_extra: String::new(),
            system_prompt_override: None,
            agent_catalog: Arc::new(std::sync::RwLock::new(builtin_subagent_profiles())),
            auto_approve_tools: false,
            context_window_override: None,
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            checkpoint_sink: None,
        }
    }
}

/// A user message handed to a running turn, to be appended at the next step
/// boundary. The id is minted by [`AgentHandle::steer`] so the host can match
/// the later [`HarnessEvent::SteerDrained`] against its own pending entry.
#[derive(Debug, Clone)]
pub struct SteerInput {
    pub(crate) id: String,
    pub(crate) text: String,
}

/// Cloneable control surface for the UI.
#[derive(Clone)]
pub struct AgentHandle {
    session: SharedSession,
    cancel: Arc<Mutex<CancellationToken>>,
    steers: Arc<Mutex<Vec<SteerInput>>>,
}

impl AgentHandle {
    pub fn session(&self) -> SharedSession {
        self.session.clone()
    }

    pub fn interrupt(&self) {
        self.cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel();
    }

    pub fn is_running(&self) -> bool {
        lock_session(&self.session).status.is_busy()
    }

    /// Hand a user message to the running turn ("steer"). It is appended to
    /// the transcript at the next step boundary — never mid-stream — and the
    /// turn keeps running to address it. Returns the steer id.
    pub fn steer(&self, text: impl Into<String>) -> String {
        let steer = SteerInput {
            id: new_id(),
            text: text.into().trim().to_string(),
        };
        self.steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(steer.clone());
        steer.id
    }

    /// Steers that never reached a step boundary (interrupt, hard failure).
    /// The host re-queues them as ordinary pending messages; the harness queue
    /// is drained so nothing survives into an unrelated later turn.
    pub fn take_pending_steers(&self) -> Vec<String> {
        self.steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .map(|steer| steer.text)
            .collect()
    }
}

pub struct AgentHarness {
    session: SharedSession,
    provider: Arc<dyn ModelProvider>,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    config: HarnessConfig,
    events: UnboundedSender<HarnessEvent>,
    cancel: Arc<Mutex<CancellationToken>>,
    steers: Arc<Mutex<Vec<SteerInput>>>,
    subagents: Option<Arc<dyn crate::subagent::SubagentSpawner>>,
}

#[derive(Default)]
struct StreamState {
    message_id: Option<String>,
    text: String,
    reasoning: String,
}

impl AgentHarness {
    pub fn new(
        session: SharedSession,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<ToolRegistry>,
        approval: Arc<dyn ApprovalGate>,
        config: HarnessConfig,
        events: UnboundedSender<HarnessEvent>,
    ) -> Self {
        Self {
            session,
            provider,
            tools,
            approval,
            config,
            events,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
            steers: Arc::new(Mutex::new(Vec::new())),
            subagents: None,
        }
    }

    /// Expose the `agent` tool's spawner to this harness and its nested tool
    /// executor.
    pub fn with_subagents(mut self, spawner: Arc<dyn crate::subagent::SubagentSpawner>) -> Self {
        self.subagents = Some(spawner);
        self
    }

    pub fn handle(&self) -> AgentHandle {
        AgentHandle {
            session: self.session.clone(),
            cancel: self.cancel.clone(),
            steers: self.steers.clone(),
        }
    }

    pub fn session(&self) -> SharedSession {
        self.session.clone()
    }

    fn emit(&self, event: HarnessEvent) {
        let _ = self.events.send(event);
    }

    fn reset_cancel_token(&self) {
        *self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = CancellationToken::new();
    }

    fn cancel_token(&self) -> CancellationToken {
        self.cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn context_window(&self) -> usize {
        self.config
            .context_window_override
            .unwrap_or_else(|| self.provider.context_window())
    }

    fn has_pending_steers(&self) -> bool {
        !self
            .steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// Append every steered user message at this step boundary. Called only
    /// from the step loop: a stream in flight or a tool mid-execution is never
    /// interrupted by an injection, matching the way a tool result and the
    /// next request stay a contiguous unit.
    fn drain_steers(&self) {
        let pending: Vec<SteerInput> = self
            .steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect();
        for steer in pending {
            let message = Message::mid_turn_user(steer.text);
            let message_id = message.id.clone();
            {
                let mut session = lock_session(&self.session);
                session.push_message(message);
            }
            self.emit(HarnessEvent::SteerDrained {
                steer_id: steer.id,
                message_id,
            });
        }
    }

    fn system_prompt(&self) -> String {
        if let Some(prompt) = &self.config.system_prompt_override {
            return prompt.clone();
        }
        let (workspace, model) = {
            let session = lock_session(&self.session);
            (session.cwd.clone(), session.model.clone())
        };
        let tools = self.tools.names();
        let agent_catalog = self
            .config
            .agent_catalog
            .read()
            .map(|catalog| catalog.clone())
            .unwrap_or_default();
        system_prompt(&PromptContext {
            workspace: &workspace,
            model: &model,
            tools: &tools,
            extra: &self.config.system_prompt_extra,
            agent_catalog: &agent_catalog,
        })
    }

    /// Run one user turn to completion.
    ///
    /// Returns an outcome instead of an error for anything that happened
    /// *inside* the loop; `Err` is reserved for caller mistakes (busy session,
    /// empty prompt).
    pub async fn run_turn(&self, input: impl Into<String>) -> Result<RunOutcome, AgentError> {
        let input = input.into().trim().to_string();
        if input.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        self.run_turn_inner(Some(input)).await
    }

    /// Continue the existing transcript without appending a new user message.
    /// Background-task notifications are appended by the host and then this
    /// method wakes the model in a normal turn.
    pub async fn run_pending(&self) -> Result<RunOutcome, AgentError> {
        if lock_session(&self.session).messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        self.run_turn_inner(None).await
    }

    async fn run_turn_inner(&self, input: Option<String>) -> Result<RunOutcome, AgentError> {
        let run_id = new_id();
        let session_id = {
            let mut session = lock_session(&self.session);
            if session.status.is_busy() {
                return Err(AgentError::Busy);
            }
            session.set_status(RunStatus::Running);
            session.steps = 0;
            session.last_error = None;
            if let Some(input) = input {
                session.push_message(Message::user(input));
            }
            session.id.clone()
        };

        self.reset_cancel_token();
        let cancel = self.cancel_token();
        self.emit(HarnessEvent::RunStarted {
            run_id: run_id.clone(),
            session_id,
        });

        let outcome = self.run_loop(&run_id, &cancel).await;

        {
            let mut session = lock_session(&self.session);
            session.set_status(RunStatus::Idle);
            if let RunOutcome::Failed(message) = &outcome {
                session.last_error = Some(message.clone());
            }
            session.touch();
        }
        self.reset_cancel_token();
        self.emit(HarnessEvent::RunFinished {
            run_id,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    async fn run_loop(&self, run_id: &str, cancel: &CancellationToken) -> RunOutcome {
        let approved = Arc::new(Mutex::new(HashSet::new()));
        let executor = ToolExecutor::new(self, approved, 0);
        let context_window = self.context_window();

        let mut step = 0usize;
        loop {
            step += 1;
            if cancel.is_cancelled() {
                return RunOutcome::Interrupted;
            }

            // Step boundary: steered user messages enter here, after the
            // previous step's tool results (or assistant text) are fully
            // committed and before the next request is built.
            self.drain_steers();

            {
                let mut session = lock_session(&self.session);
                session.steps = step;
                session.touch();
            }
            self.emit(HarnessEvent::StepStarted {
                run_id: run_id.to_string(),
                step,
            });

            let system_prompt = self.system_prompt();

            // Compact before building the request so the estimate is current.
            let compacted = {
                let mut session = lock_session(&self.session);
                context::maybe_compact(
                    &mut session,
                    context_window,
                    self.config.compact_threshold,
                    self.config.keep_recent_turns,
                    &system_prompt,
                    self.config.max_summary_chars,
                )
            };
            if let Some(record) = compacted {
                self.emit(HarnessEvent::ContextCompacted { record });
            }

            let request = {
                let session = lock_session(&self.session);
                ChatRequest {
                    model: session.model.clone(),
                    messages: context::build_chat_messages(&session, &system_prompt),
                    tools: self.tools.specs(),
                    temperature: self.config.temperature,
                    max_tokens: self.config.max_tokens,
                    reasoning_effort: self.config.reasoning_effort.clone(),
                }
            };

            let mut stream = StreamState::default();
            let completion = match self.stream_with_retry(request, &mut stream, cancel).await {
                Ok(Some(completion)) => completion,
                Ok(None) => {
                    self.finish_stream(&mut stream, true);
                    return RunOutcome::Interrupted;
                }
                Err(error) => {
                    self.finish_stream(&mut stream, true);
                    return RunOutcome::Failed(error.to_string());
                }
            };

            if cancel.is_cancelled() {
                self.finish_stream(&mut stream, true);
                return RunOutcome::Interrupted;
            }

            let tool_calls = self.commit_completion(&mut stream, completion);
            if tool_calls.is_empty() {
                // A steer that arrived while the final text was streaming
                // extends the turn: the assistant message above stays in the
                // transcript and the model gets another request with the new
                // user input appended (ZCode's text-only continuation edge).
                if self.has_pending_steers() {
                    continue;
                }
                return RunOutcome::Completed;
            }

            let mut interrupted = false;
            for (index, call) in tool_calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    interrupted = true;
                }

                let (result, was_cancelled) = if interrupted {
                    (ToolResult::cancelled(call), true)
                } else {
                    executor.execute(call, cancel).await
                };

                {
                    let mut session = lock_session(&self.session);
                    session.push_message(Message::from_tool_result(result.clone()));
                }
                self.emit(HarnessEvent::ToolCallFinished {
                    result: result.clone(),
                });

                if was_cancelled {
                    interrupted = true;
                    for remaining in &tool_calls[index + 1..] {
                        let result = ToolResult::cancelled(remaining);
                        {
                            let mut session = lock_session(&self.session);
                            session.push_message(Message::from_tool_result(result.clone()));
                        }
                        self.emit(HarnessEvent::ToolCallFinished { result });
                    }
                    break;
                }

                if cancel.is_cancelled() {
                    interrupted = true;
                }
            }

            if interrupted {
                return RunOutcome::Interrupted;
            }
        }
    }

    /// Stream one completion, retrying only when nothing has been shown yet.
    async fn stream_with_retry(
        &self,
        request: ChatRequest,
        stream: &mut StreamState,
        cancel: &CancellationToken,
    ) -> Result<Option<Completion>, ProviderError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.stream_once(request.clone(), stream, cancel).await {
                Ok(Some(completion)) => return Ok(Some(completion)),
                Ok(None) => return Ok(None),
                Err(error) if error.is_cancelled() => return Ok(None),
                Err(error) => {
                    let can_retry = stream.text.is_empty()
                        && stream.reasoning.is_empty()
                        && attempt <= self.config.max_retries
                        && error.is_retryable();
                    if !can_retry {
                        return Err(error);
                    }
                    let delay = self.config.retry_base_delay * 2u32.pow((attempt - 1) as u32);
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(None),
                        _ = sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn stream_once(
        &self,
        request: ChatRequest,
        stream: &mut StreamState,
        cancel: &CancellationToken,
    ) -> Result<Option<Completion>, ProviderError> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut receiver_open = true;
        let future = self.provider.stream(request, cancel.clone(), sender);
        tokio::pin!(future);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(None),
                event = receiver.recv(), if receiver_open => {
                    match event {
                        Some(event) => self.apply_provider_event(stream, event),
                        None => receiver_open = false,
                    }
                }
                result = &mut future => {
                    // The provider may have queued deltas before returning.
                    while let Ok(event) = receiver.try_recv() {
                        self.apply_provider_event(stream, event);
                    }
                    return result.map(Some);
                }
            }
        }
    }

    fn apply_provider_event(&self, stream: &mut StreamState, event: ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(delta) => {
                if delta.is_empty() {
                    return;
                }
                let message_id = self.ensure_assistant(stream);
                {
                    let mut session = lock_session(&self.session);
                    if let Some(message) = session.message_by_id(&message_id) {
                        message.push_text(&delta);
                    }
                    session.touch();
                }
                stream.text.push_str(&delta);
                self.emit(HarnessEvent::TextDelta { message_id, delta });
            }
            ProviderEvent::ReasoningDelta(delta) => {
                if delta.is_empty() {
                    return;
                }
                let message_id = self.ensure_assistant(stream);
                {
                    let mut session = lock_session(&self.session);
                    if let Some(message) = session.message_by_id(&message_id) {
                        message.push_reasoning(&delta);
                    }
                    session.touch();
                }
                stream.reasoning.push_str(&delta);
                self.emit(HarnessEvent::ReasoningDelta { message_id, delta });
            }
            ProviderEvent::Usage(usage) => {
                let total = {
                    let mut session = lock_session(&self.session);
                    session.add_usage(&usage);
                    session.usage.clone()
                };
                self.emit(HarnessEvent::UsageUpdated { usage: total });
            }
        }
    }

    fn ensure_assistant(&self, stream: &mut StreamState) -> String {
        if let Some(id) = &stream.message_id {
            return id.clone();
        }
        let message = Message::assistant();
        let message_id = message.id.clone();
        {
            let mut session = lock_session(&self.session);
            session.push_message(message);
        }
        stream.message_id = Some(message_id.clone());
        self.emit(HarnessEvent::AssistantStarted {
            message_id: message_id.clone(),
        });
        message_id
    }

    /// Reconcile streamed deltas with the provider's authoritative completion,
    /// persist tool calls and close the assistant message.
    fn commit_completion(&self, stream: &mut StreamState, completion: Completion) -> Vec<ToolCall> {
        let final_text = if completion.text.is_empty() {
            stream.text.clone()
        } else {
            completion.text.clone()
        };
        let final_reasoning = if completion.reasoning.is_empty() {
            stream.reasoning.clone()
        } else {
            completion.reasoning.clone()
        };

        let needs_message = stream.message_id.is_some()
            || !final_text.is_empty()
            || !final_reasoning.is_empty()
            || !completion.tool_calls.is_empty();
        if !needs_message {
            let usage = {
                let mut session = lock_session(&self.session);
                session.add_usage(&completion.usage);
                session.usage.clone()
            };
            self.emit(HarnessEvent::UsageUpdated { usage });
            return Vec::new();
        }

        let message_id = self.ensure_assistant(stream);
        {
            let mut session = lock_session(&self.session);
            if let Some(message) = session.message_by_id(&message_id) {
                if !final_text.is_empty() {
                    message.set_text(final_text.clone());
                }
                if !final_reasoning.is_empty() {
                    message.set_reasoning(final_reasoning.clone());
                }
                for call in &completion.tool_calls {
                    message.push_tool_call(call.clone());
                }
            }
            session.add_usage(&completion.usage);
            session.touch();
        }

        // Providers that do not stream deltas still need to drive the UI.
        if stream.text.is_empty() && !final_text.is_empty() {
            self.emit(HarnessEvent::TextDelta {
                message_id: message_id.clone(),
                delta: final_text.clone(),
            });
        }
        if stream.reasoning.is_empty() && !final_reasoning.is_empty() {
            self.emit(HarnessEvent::ReasoningDelta {
                message_id: message_id.clone(),
                delta: final_reasoning.clone(),
            });
        }

        stream.text = final_text;
        stream.reasoning = final_reasoning;
        self.finish_stream(stream, false);
        let usage = lock_session(&self.session).usage.clone();
        self.emit(HarnessEvent::UsageUpdated { usage });
        completion.tool_calls
    }

    fn finish_stream(&self, stream: &mut StreamState, interrupted: bool) {
        let Some(message_id) = &stream.message_id else {
            return;
        };
        if interrupted {
            let mut session = lock_session(&self.session);
            if let Some(message) = session.message_by_id(message_id) {
                message.interrupted = true;
            }
            session.touch();
        }
        self.emit(HarnessEvent::AssistantFinished {
            message_id: message_id.clone(),
            interrupted,
        });
    }
}

/// Executes tool calls outside the model loop.
///
/// Keeping this separate from [`AgentHarness`] allows a tool (such as the PTC
/// JavaScript runner) to invoke other tools through the same approval,
/// timeout, panic-isolation and truncation path.
#[derive(Clone)]
struct ToolExecutor {
    session: SharedSession,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    config: HarnessConfig,
    events: UnboundedSender<HarnessEvent>,
    approved: Arc<Mutex<HashSet<String>>>,
    depth: usize,
    subagents: Option<Arc<dyn crate::subagent::SubagentSpawner>>,
}

impl ToolExecutor {
    fn new(harness: &AgentHarness, approved: Arc<Mutex<HashSet<String>>>, depth: usize) -> Self {
        Self {
            session: harness.session.clone(),
            tools: harness.tools.clone(),
            approval: harness.approval.clone(),
            config: harness.config.clone(),
            events: harness.events.clone(),
            approved,
            depth,
            subagents: harness.subagents.clone(),
        }
    }

    fn child(&self) -> Self {
        let mut child = self.clone();
        child.depth += 1;
        child
    }

    fn emit(&self, event: HarnessEvent) {
        let _ = self.events.send(event);
    }

    async fn execute(&self, call: &ToolCall, cancel: &CancellationToken) -> (ToolResult, bool) {
        if let Some(parse_error) = &call.argument_parse_error {
            let result = ToolResult::failure(
                call,
                format!("Tool arguments were not valid JSON: {parse_error}"),
            );
            self.emit(HarnessEvent::ToolCallFinished {
                result: result.clone(),
            });
            return (result, false);
        }

        let Some(tool) = self.tools.get(&call.name) else {
            let result = ToolResult::failure(
                call,
                format!(
                    "Unknown tool `{}`. Available tools: {}.",
                    call.name,
                    if self.tools.is_empty() {
                        "none".to_string()
                    } else {
                        self.tools.names().join(", ")
                    }
                ),
            );
            self.emit(HarnessEvent::ToolCallFinished {
                result: result.clone(),
            });
            return (result, false);
        };

        let policy = tool.approval();
        if policy == ApprovalPolicy::Deny {
            let result = ToolResult::failure(call, "This tool is disabled by policy.");
            self.emit(HarnessEvent::ToolCallFinished {
                result: result.clone(),
            });
            return (result, false);
        }

        let permission_mode = {
            let session = lock_session(&self.session);
            session.permission_mode
        };
        let already_approved = self
            .approved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&call.name);
        // Plan mode consults the gate for every tool regardless of its declared
        // policy: Auto tools such as `run_js` and `agent` must not bypass the
        // read-only research contract, and the session approval cache ("allow
        // for this session") must not pre-approve calls collected in another
        // mode. The gate decides locally for read-only tools, so this adds no
        // prompts for reads in Plan.
        let needs_approval = permission_mode == PermissionMode::Plan
            || (policy == ApprovalPolicy::Ask
                && !self.config.auto_approve_tools
                && !already_approved);

        if needs_approval {
            let request = ApprovalRequest::new(call, tool.summarize(&call.arguments));
            {
                let mut session = lock_session(&self.session);
                session.set_status(RunStatus::WaitingApproval);
            }

            let decision = tokio::select! {
                _ = cancel.cancelled() => {
                    let result = ToolResult::cancelled(call);
                    self.emit(HarnessEvent::ToolCallFinished { result: result.clone() });
                    self.emit(HarnessEvent::ApprovalResolved {
                        request_id: request.id.clone(),
                        decision: ApprovalDecision::Deny,
                    });
                    {
                        let mut session = lock_session(&self.session);
                        session.set_status(RunStatus::Running);
                    }
                    return (result, true);
                }
                decision = self.approval.request_approval(request.clone()) => decision,
            };

            {
                let mut session = lock_session(&self.session);
                session.set_status(RunStatus::Running);
            }
            self.emit(HarnessEvent::ApprovalResolved {
                request_id: request.id.clone(),
                decision,
            });

            match decision {
                ApprovalDecision::Approve => {}
                ApprovalDecision::ApproveForSession => {
                    self.approved
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(call.name.clone());
                }
                ApprovalDecision::Deny => {
                    let denial = if permission_mode == PermissionMode::Plan {
                        "The user denied this tool call because permission mode is Plan: file \
                         edits and command execution are blocked so you can research read-only. \
                         Produce a plan first and wait for the user to switch the permission mode \
                         before acting. Do not retry this call."
                    } else {
                        "The user denied this tool call. Do not retry it; explain what you would do instead."
                    };
                    let result = ToolResult::failure(call, denial);
                    self.emit(HarnessEvent::ToolCallFinished {
                        result: result.clone(),
                    });
                    return (result, false);
                }
            }
        }

        if cancel.is_cancelled() {
            let result = ToolResult::cancelled(call);
            self.emit(HarnessEvent::ToolCallFinished {
                result: result.clone(),
            });
            return (result, true);
        }

        self.emit(HarnessEvent::ToolCallStarted { call: call.clone() });
        let started = Instant::now();
        let (cwd, session_id) = {
            let session = lock_session(&self.session);
            (session.cwd.clone(), session.id.clone())
        };
        let ctx = ToolContext {
            cwd,
            session_id,
            agent_id: self.config.agent_id.clone(),
            agent_name: self.config.agent_name.clone(),
            agent_depth: self.config.agent_depth,
            call_id: call.id.clone(),
            cancel: cancel.clone(),
            nested: (self.depth < 2).then(|| {
                Arc::new(NestedInvoker {
                    executor: self.child(),
                }) as Arc<dyn crate::tool::NestedToolInvoker>
            }),
            subagents: self.subagents.clone(),
            checkpoint_sink: self.config.checkpoint_sink.clone(),
        };

        let execution =
            std::panic::AssertUnwindSafe(tool.execute(ctx, call.arguments.clone())).catch_unwind();
        let outcome = tokio::select! {
            _ = cancel.cancelled() => Err(ToolRunFailure::Cancelled),
            timed = tokio::time::timeout(self.config.tool_timeout, execution) => {
                match timed {
                    Err(_) => Err(ToolRunFailure::Failed(ToolError::new(format!(
                        "Tool `{}` timed out after {:?}.",
                        call.name, self.config.tool_timeout
                    )))),
                    Ok(Err(_panic)) => Err(ToolRunFailure::Failed(ToolError::new(format!(
                        "Tool `{}` panicked. The run continues; do not assume the action happened.",
                        call.name
                    )))),
                    Ok(Ok(Err(error))) => Err(ToolRunFailure::Failed(error)),
                    Ok(Ok(Ok(output))) => Ok(output),
                }
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Err(ToolRunFailure::Cancelled) => {
                let result = ToolResult::cancelled(call);
                self.emit(HarnessEvent::ToolCallFinished {
                    result: result.clone(),
                });
                (result, true)
            }
            Err(ToolRunFailure::Failed(error)) => {
                let mut result = ToolResult::failure(call, error.message);
                result.duration_ms = Some(duration_ms);
                self.emit(HarnessEvent::ToolCallFinished {
                    result: result.clone(),
                });
                (result, false)
            }
            Ok(output) => {
                for effect in output.effects {
                    match effect {
                        ToolEffect::SetTodos(todos) => {
                            {
                                let mut session = lock_session(&self.session);
                                session.apply_todos(todos.clone());
                            }
                            self.emit(HarnessEvent::TodosUpdated { todos });
                        }
                    }
                }

                let content =
                    truncate_tool_output(output.content, self.config.max_tool_output_chars);
                let mut result = if output.is_error {
                    ToolResult::failure(call, content)
                } else {
                    ToolResult::success(call, content)
                };
                result.metadata = output.metadata;
                result.images = output.images;
                result.duration_ms = Some(duration_ms);
                self.emit(HarnessEvent::ToolCallFinished {
                    result: result.clone(),
                });
                (result, false)
            }
        }
    }
}

struct NestedInvoker {
    executor: ToolExecutor,
}

#[async_trait::async_trait]
impl crate::tool::NestedToolInvoker for NestedInvoker {
    fn tool_names(&self) -> Vec<String> {
        self.executor.tools.names()
    }

    async fn invoke(
        &self,
        name: &str,
        arguments: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        if name == "run_js" {
            return Err("Nested JavaScript execution is not allowed.".to_string());
        }
        let call = ToolCall {
            id: format!("nested_{}", new_id()),
            name: name.to_string(),
            arguments,
            argument_parse_error: None,
        };
        let (result, cancelled) = self.executor.execute(&call, cancel).await;
        if cancelled {
            return Err("cancelled".to_string());
        }
        if result.is_error {
            Err(format!("`{}` failed: {}", result.name, result.content))
        } else {
            Ok(result.content)
        }
    }
}

enum ToolRunFailure {
    Cancelled,
    Failed(ToolError),
}

fn truncate_tool_output(content: String, max_chars: usize) -> String {
    let count = content.chars().count();
    if count <= max_chars {
        return content;
    }
    let kept: String = content.chars().take(max_chars).collect();
    format!(
        "{kept}\n\n[Output truncated: {} of {} characters omitted. Use narrower queries or offsets.]",
        count - max_chars,
        count
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Role, TodoStatus};
    use crate::provider::ToolSpec;
    use crate::session::{SessionState, new_shared_session};
    use crate::testing::ScriptedProvider;
    use crate::tool::{AutoApprovalGate, Tool, ToolOutput};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use tokio::sync::mpsc::UnboundedReceiver;

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
            let text = arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ToolOutput::text(text))
        }
    }

    struct TodoTool;

    #[async_trait]
    impl Tool for TodoTool {
        fn name(&self) -> &str {
            "todo_write"
        }
        fn description(&self) -> &str {
            "Set the todo list"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(
                ToolOutput::text("updated").with_effect(ToolEffect::SetTodos(vec![TodoItem::new(
                    "write tests",
                    TodoStatus::InProgress,
                )])),
            )
        }
    }

    fn harness_with(
        script: Vec<Completion>,
        tools: ToolRegistry,
        config: HarnessConfig,
    ) -> (
        AgentHarness,
        AgentHandle,
        UnboundedReceiver<HarnessEvent>,
        Arc<ScriptedProvider>,
    ) {
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let provider = Arc::new(ScriptedProvider::new(script));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session,
            provider.clone(),
            Arc::new(tools),
            Arc::new(AutoApprovalGate),
            config,
            tx,
        );
        let handle = harness.handle();
        (harness, handle, rx, provider)
    }

    fn default_tools() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        tools.register(TodoTool);
        tools
    }

    #[tokio::test]
    async fn simple_turn_completes_and_records_messages() {
        let (harness, handle, mut events, _) = harness_with(
            vec![Completion::text("Hello there")],
            default_tools(),
            HarnessConfig::default(),
        );
        let outcome = harness.run_turn("hi").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        {
            let shared = handle.session();
            let session = lock_session(&shared);
            assert_eq!(session.messages.len(), 2);
            assert_eq!(session.messages[0].role, Role::User);
            assert_eq!(session.messages[1].role, Role::Assistant);
            assert_eq!(session.messages[1].text(), "Hello there");
            assert_eq!(session.title, "hi");
        }
        let mut saw_finished = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HarnessEvent::RunFinished { .. }) {
                saw_finished = true;
            }
        }
        assert!(saw_finished);
    }

    #[tokio::test]
    async fn run_pending_resumes_without_appending_a_user_message() {
        let (harness, handle, _, provider) = harness_with(
            vec![Completion::text("woken")],
            default_tools(),
            HarnessConfig::default(),
        );
        {
            let shared = harness.session();
            let mut session = lock_session(&shared);
            session.push_message(Message::synthetic_user(
                "<task-notification>agent finished</task-notification>",
            ));
        }
        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.role == "user"
                    && message.content.contains("task-notification"))
        );
        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].text(), "woken");
    }

    #[tokio::test]
    async fn tool_calls_are_executed_and_result_is_appended() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool {
            calls: calls.clone(),
        });
        let (harness, handle, _, _) = harness_with(
            vec![
                Completion::default()
                    .with_tool_call(ToolCall::new("echo", json!({"text": "ping"}))),
                Completion::text("done"),
            ],
            tools,
            HarnessConfig::default(),
        );
        let outcome = harness.run_turn("use the tool").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(&*calls.lock().unwrap(), &[json!({"text": "ping"})]);
        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[2].role, Role::Tool);
        assert_eq!(session.messages[2].tool_result().unwrap().content, "ping");
        assert_eq!(session.messages[3].text(), "done");
    }

    /// An `Auto`-policy tool named `run_js` so it classifies as `Command`:
    /// exactly the kind of tool that used to bypass Plan mode because the
    /// harness only consulted the gate for `Ask`-policy tools.
    struct RunJsProbeTool {
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Tool for RunJsProbeTool {
        fn name(&self) -> &str {
            "run_js"
        }
        fn description(&self) -> &str {
            "Run a sandboxed script"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.lock().unwrap().push(json!({}));
            Ok(ToolOutput::text("executed"))
        }
    }

    /// Regression: Plan mode consults the gate even for `Auto`-policy tools,
    /// so `run_js` cannot bypass the read-only research contract, and the
    /// denial tells the model to produce a plan first.
    #[tokio::test]
    async fn plan_mode_gates_auto_policy_tools_with_plan_denial_text() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(RunJsProbeTool {
            calls: calls.clone(),
        });
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        lock_session(&session).permission_mode = PermissionMode::Plan;
        let mode_session = session.clone();
        let gate = Arc::new(crate::tool::ModeApprovalGate::new(
            Arc::new(AutoApprovalGate),
            Arc::new(move || lock_session(&mode_session).permission_mode),
        ));
        let provider = Arc::new(ScriptedProvider::new(vec![
            Completion::default().with_tool_call(ToolCall::new("run_js", json!({}))),
            Completion::text("done"),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider,
            Arc::new(tools),
            gate,
            HarnessConfig::default(),
            tx,
        );
        let outcome = harness.run_turn("research only").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert!(
            calls.lock().unwrap().is_empty(),
            "plan mode must not execute Auto-policy command tools"
        );
        let stored = lock_session(&session);
        let result = stored.messages[2].tool_result().unwrap();
        assert!(result.is_error);
        assert!(
            result.content.contains("Plan"),
            "denial should explain plan mode: {}",
            result.content
        );
    }

    /// A gate that approves the first request "for this session" — and, like a
    /// real user, switches the session into Plan mode right after. Later
    /// requests are denied by the gate itself.
    struct ApproveThenSwitchToPlanGate {
        asks: Arc<Mutex<usize>>,
        session: SharedSession,
    }

    #[async_trait]
    impl ApprovalGate for ApproveThenSwitchToPlanGate {
        async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
            let mut asks = self
                .asks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *asks += 1;
            if *asks == 1 {
                lock_session(&self.session).permission_mode = PermissionMode::Plan;
                ApprovalDecision::ApproveForSession
            } else {
                ApprovalDecision::Deny
            }
        }
    }

    /// Regression: the "allow for this session" cache collected in Default
    /// mode must not bypass Plan mode within the same run.
    #[tokio::test]
    async fn plan_mode_ignores_session_approval_cache() {
        struct AskShellTool;

        #[async_trait]
        impl Tool for AskShellTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "Run a command"
            }
            fn parameters(&self) -> Value {
                json!({"type": "object"})
            }
            fn approval(&self) -> ApprovalPolicy {
                ApprovalPolicy::Ask
            }
            async fn execute(
                &self,
                _ctx: ToolContext,
                _arguments: Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("ran"))
            }
        }

        let mut tools = ToolRegistry::new();
        tools.register(AskShellTool);
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let asks = Arc::new(Mutex::new(0));
        let gate = Arc::new(ApproveThenSwitchToPlanGate {
            asks: asks.clone(),
            session: session.clone(),
        });
        let provider = Arc::new(ScriptedProvider::new(vec![
            Completion::default().with_tool_call(ToolCall::new("shell", json!({}))),
            Completion::default().with_tool_call(ToolCall::new("shell", json!({}))),
            Completion::text("done"),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider,
            Arc::new(tools),
            gate,
            HarnessConfig::default(),
            tx,
        );
        let outcome = harness.run_turn("run twice").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        // The second call must have reached the gate (which switched the
        // session to Plan and then denied) instead of the cache short-circuit.
        assert_eq!(
            &*asks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            &2
        );
        let stored = lock_session(&session);
        let result = stored.messages[4].tool_result().unwrap();
        assert!(result.is_error);
        assert!(
            result.content.contains("Plan"),
            "denial should explain plan mode: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn tool_effects_update_session_todos() {
        let (harness, handle, _, _) = harness_with(
            vec![
                Completion::default().with_tool_call(ToolCall::new("todo_write", json!({}))),
                Completion::text("plan saved"),
            ],
            default_tools(),
            HarnessConfig::default(),
        );
        harness.run_turn("make a plan").await.unwrap();
        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(session.todos.len(), 1);
        assert_eq!(session.todos[0].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn unknown_tool_becomes_an_error_observation() {
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let (harness, handle, _, _) = harness_with(
            vec![
                Completion::default().with_tool_call(ToolCall::new("nope", json!({}))),
                Completion::text("recovered"),
            ],
            tools,
            HarnessConfig::default(),
        );
        let outcome = harness.run_turn("try it").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        let shared = handle.session();
        let session = lock_session(&shared);
        let result = session.messages[2].tool_result().unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn runs_until_the_model_stops_calling_tools() {
        // 26 tool steps + one final text step: the loop must not stop at the
        // old per-turn cap (24) — there is no step limit any more.
        let mut script = (0..26)
            .map(|index| {
                Completion::default()
                    .with_tool_call(ToolCall::new("echo", json!({"text": index.to_string()})))
            })
            .collect::<Vec<_>>();
        script.push(Completion::text("done"));
        let (harness, handle, _, _) =
            harness_with(script, default_tools(), HarnessConfig::default());
        let outcome = harness.run_turn("loop").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(session.steps, 27);
    }

    #[tokio::test]
    async fn interrupt_is_reported_and_keeps_partial_output() {
        let (harness, handle, _, _) = harness_with(
            vec![Completion::text("this is a long response")],
            default_tools(),
            HarnessConfig::default(),
        );
        let run = harness.run_turn("go");
        let interrupter = {
            let handle = handle.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                handle.interrupt();
            }
        };
        let (outcome, _) = tokio::join!(run, interrupter);
        let outcome = outcome.unwrap();
        assert_eq!(outcome, RunOutcome::Interrupted);
        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(session.status, RunStatus::Idle);
        assert!(session.messages.iter().any(|m| m.interrupted));
    }

    #[tokio::test]
    async fn provider_request_includes_tool_specs_and_system_prompt() {
        let (harness, _, _, provider) = harness_with(
            vec![Completion::text("ok")],
            default_tools(),
            HarnessConfig::default(),
        );
        harness.run_turn("hello").await.unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.model, "test-model");
        assert!(request.messages[0].role == "system");
        assert!(request.messages[0].content.contains("EffiBuddy"));
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|t: &ToolSpec| t.name.as_str())
            .collect();
        assert!(names.contains(&"echo"));
    }

    /// A tool that parks until the test releases it, so the steer lands while
    /// a tool is mid-execution — the harshest possible injection timing.
    struct GateTool {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        _release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Tool for GateTool {
        fn name(&self) -> &str {
            "gate"
        }
        fn description(&self) -> &str {
            "Block until released"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            let _ = self.entered.send(());
            self._release.notified().await;
            Ok(ToolOutput::text("released"))
        }
    }

    /// Regression: a steer handed to a running turn enters the transcript at
    /// the next step boundary (after the tool result is committed), is framed
    /// with the mid-turn template in the next request, and never interrupts
    /// the tool itself.
    #[tokio::test]
    async fn steer_is_injected_at_the_next_step_boundary() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let mut tools = ToolRegistry::new();
        tools.register(GateTool {
            entered: entered_tx,
            _release: release.clone(),
        });
        let (harness, handle, mut events, provider) = harness_with(
            vec![
                Completion::default().with_tool_call(ToolCall::new("gate", json!({}))),
                Completion::text("done"),
            ],
            tools,
            HarnessConfig::default(),
        );
        let requests_provider = provider.clone();
        let run = tokio::spawn(async move { harness.run_turn("start").await });

        entered_rx.recv().await.expect("tool reported entry");
        let steer_id = handle.steer("mid-run");
        release.notify_one();
        let outcome = run.await.unwrap().unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        {
            let shared = handle.session();
            let session = lock_session(&shared);
            assert_eq!(session.messages.len(), 5);
            assert!(session.messages[3].mid_turn, "steer is flagged mid_turn");
            assert_eq!(session.messages[3].text(), "mid-run");
        }
        let requests = requests_provider.requests();
        assert_eq!(requests.len(), 2);
        let second = requests[1]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            second.contains("<user-steer>"),
            "wire frame missing: {second}"
        );
        assert!(second.contains("mid-run"));

        let mut saw_drained = false;
        while let Ok(event) = events.try_recv() {
            if matches!(
                &event,
                HarnessEvent::SteerDrained { steer_id: drained, .. } if drained.as_str() == steer_id
            ) {
                saw_drained = true;
            }
        }
        assert!(saw_drained, "host must be told which steer was drained");
    }

    /// Provider whose first response waits on a test gate, so a steer can be
    /// submitted while the final (text-only) answer is streaming.
    struct GatedTextProvider {
        script: Mutex<std::collections::VecDeque<Completion>>,
        requests: Mutex<Vec<ChatRequest>>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl GatedTextProvider {
        fn new(script: Vec<Completion>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ModelProvider for GatedTextProvider {
        fn name(&self) -> &str {
            "gated-text"
        }
        fn context_window(&self) -> usize {
            32_768
        }
        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
            _events: UnboundedSender<ProviderEvent>,
        ) -> Result<Completion, ProviderError> {
            let first = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len() == 1
            };
            if first {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::Other("gated provider ran dry".into()))
        }
    }

    /// Regression: a steer that arrives while the model is streaming its final
    /// text extends the turn instead of being dropped — the answer above stays
    /// in the transcript and the model gets another request with the input.
    #[tokio::test]
    async fn steer_extends_a_turn_that_finished_its_text() {
        let provider = Arc::new(GatedTextProvider::new(vec![
            Completion::text("first answer"),
            Completion::text("second answer"),
        ]));
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session,
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            Arc::new(AutoApprovalGate),
            HarnessConfig::default(),
            tx,
        );
        let handle = harness.handle();
        let run = tokio::spawn(async move { harness.run_turn("q").await });

        provider.entered.notified().await;
        handle.steer("extra");
        provider.release.notify_one();
        let outcome = run.await.unwrap().unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(provider.requests().len(), 2);
        let shared = handle.session();
        let session = lock_session(&shared);
        let texts: Vec<String> = session
            .messages
            .iter()
            .map(|message| (message.role, message.text()))
            .filter(|(role, _)| *role == Role::User || *role == Role::Assistant)
            .map(|(_, text)| text)
            .collect();
        assert_eq!(texts, vec!["q", "first answer", "extra", "second answer"]);
        assert!(session.messages[2].mid_turn);
    }

    /// Regression: interrupting a run hands unconsumed steers back to the
    /// host (which re-queues them) instead of losing the text in the harness.
    #[tokio::test]
    async fn interrupted_turn_hands_unconsumed_steers_back_to_the_host() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tools = ToolRegistry::new();
        tools.register(GateTool {
            entered: entered_tx,
            _release: Arc::new(tokio::sync::Notify::new()),
        });
        let (harness, handle, _events, _provider) = harness_with(
            vec![Completion::default().with_tool_call(ToolCall::new("gate", json!({})))],
            tools,
            HarnessConfig::default(),
        );
        let run = tokio::spawn(async move { harness.run_turn("start").await });
        entered_rx.recv().await.expect("tool reported entry");
        handle.steer("never drained");
        handle.interrupt();
        let outcome = run.await.unwrap().unwrap();
        assert_eq!(outcome, RunOutcome::Interrupted);
        assert_eq!(
            handle.take_pending_steers(),
            vec!["never drained".to_string()]
        );
    }
}
