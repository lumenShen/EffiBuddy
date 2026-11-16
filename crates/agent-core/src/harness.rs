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
//! 6. **Context is compacted at conversation-round boundaries** before every
//!    model step, so a single long tool turn is covered; the rolling summary is
//!    sent as a system block and old tool output may be cleared first.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::context;
use crate::error::{AgentError, ProviderError, ToolError};
use crate::ids::new_id;
use crate::message::{
    FinishReason, Message, TodoItem, ToolCall, ToolResult, Usage, UserAttachment,
};
use crate::prompt::{PromptContext, system_prompt};
use crate::provider::{ChatMessage, ChatRequest, Completion, ModelProvider, ProviderEvent};
use crate::session::{
    CompactionRecord, HeldCompaction, HeldCompactionStatus, RunOutcome, RunStatus, SharedSession,
    lock_session,
};
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
    /// A partially streamed assistant message was rolled back because the
    /// request failed and is being retried. The next attempt starts from a
    /// clean transcript, so the host must drop the local reveal/snapshot copy.
    AssistantDiscarded {
        message_id: String,
    },
    /// A retryable provider failure (a dropped SSE stream, a 429/5xx) is about
    /// to be retried. The host can show "正在重试" instead of an error card.
    ProviderRetry {
        attempt: usize,
        max_retries: usize,
        reason: String,
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
    /// Percentage-based context compaction policy. Resolved against the
    /// session model's context window at every step.
    pub compaction: crate::context::CompactionConfig,
    /// Cap for one tool result before it enters the transcript.
    pub max_tool_output_chars: usize,
    pub tool_timeout: Duration,
    pub max_retries: usize,
    pub retry_base_delay: Duration,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Reasoning effort requested from models that expose levels.
    pub reasoning_effort: Option<String>,
    /// System prompt fallback text (project instructions, plan-mode rules).
    pub system_prompt_extra: String,
    /// Additional project folders beyond the session cwd. Built-in tools may
    /// read and write inside any of them; the prompt lists them so the model
    /// knows the full footprint.
    pub additional_workspaces: Vec<PathBuf>,
    /// Project id of the owning session, inherited by child sessions so they
    /// can be regrouped correctly if they ever surface outside the agent panel.
    pub project_id: Option<String>,
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
    /// Live permission-mode source. `None` means "read
    /// [`crate::session::SessionState::permission_mode`] from this harness's own
    /// session", which is right for the main session. Child harnesses get the
    /// owning conversation's provider so one Shift+Tab applies to the whole
    /// agent tree — a subagent must not keep asking (or keep refusing) after
    /// the user switched mode.
    pub permission_mode: Option<Arc<dyn Fn() -> PermissionMode + Send + Sync>>,
}

impl HarnessConfig {
    /// Effective permission mode for a tool call in this harness.
    pub fn effective_permission_mode(
        &self,
        session: &crate::session::SharedSession,
    ) -> PermissionMode {
        match &self.permission_mode {
            Some(provider) => provider(),
            None => lock_session(session).permission_mode,
        }
    }
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            compaction: crate::context::CompactionConfig::default(),
            max_tool_output_chars: 24_000,
            tool_timeout: Duration::from_secs(180),
            max_retries: 2,
            retry_base_delay: Duration::from_millis(600),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            system_prompt_extra: String::new(),
            additional_workspaces: Vec::new(),
            project_id: None,
            system_prompt_override: None,
            agent_catalog: Arc::new(std::sync::RwLock::new(builtin_subagent_profiles())),
            auto_approve_tools: false,
            context_window_override: None,
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            checkpoint_sink: None,
            permission_mode: None,
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
    pub(crate) attachments: Vec<UserAttachment>,
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
        self.steer_with_attachments(text, Vec::new())
    }

    /// Steer with composer attachments (a file dropped mid-turn).
    pub fn steer_with_attachments(
        &self,
        text: impl Into<String>,
        attachments: Vec<UserAttachment>,
    ) -> String {
        let steer = SteerInput {
            id: new_id(),
            text: text.into().trim().to_string(),
            attachments,
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
    /// Consecutive summary failures. ZCode stops auto compaction after a few
    /// failures instead of retrying a doomed request at every step.
    compact_failures: AtomicUsize,
}

#[derive(Default)]
struct StreamState {
    message_id: Option<String>,
    text: String,
    reasoning: String,
    /// True when this stream resumes the previous assistant message after an
    /// output-limit cut instead of opening a new one. The transcript message
    /// already holds the earlier part; deltas append to it, so the UI keeps a
    /// single growing answer rather than two assistant rows (a merged turn
    /// would only show the last row's text).
    append: bool,
}

/// Request-only user instruction appended after an answer was cut off by the
/// model's output-token limit. The partial assistant text is already in the
/// transcript, so the model only needs to pick up where it stopped. This is
/// never persisted: it belongs to the in-flight turn, like ZCode's hidden
/// output-token continuation entry.
const OUTPUT_LIMIT_CONTINUE_PROMPT: &str = "Output token limit hit. Resume directly - no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";

/// ZCode allows three continuations per user turn; after that the output
/// limit is reported as an error instead of silently ending mid-answer.
const MAX_OUTPUT_CONTINUATIONS: usize = 3;

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
            compact_failures: AtomicUsize::new(0),
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
            let message = Message::mid_turn_user_with_attachments(steer.text, steer.attachments);
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
            additional_workspaces: &self.config.additional_workspaces,
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
        self.run_turn_with_attachments(input, Vec::new()).await
    }

    /// Start a turn whose user message carries composer attachments (files,
    /// images or activated skills). An attachment-only message is valid: the
    /// user can drop a file and send it without typing anything.
    pub async fn run_turn_with_attachments(
        &self,
        input: impl Into<String>,
        attachments: Vec<UserAttachment>,
    ) -> Result<RunOutcome, AgentError> {
        let input = input.into().trim().to_string();
        if input.is_empty() && attachments.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        self.run_turn_inner(Some(input), attachments).await
    }

    /// Continue the existing transcript without appending a new user message.
    /// Background-task notifications are appended by the host and then this
    /// method wakes the model in a normal turn.
    pub async fn run_pending(&self) -> Result<RunOutcome, AgentError> {
        if lock_session(&self.session).messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        self.run_turn_inner(None, Vec::new()).await
    }

    /// Compact the transcript on demand — the composer's `/compact` command.
    ///
    /// Uses the same pipeline as the automatic path (microcompact, then a
    /// model-written rolling summary with an extractive fallback) but ignores
    /// the token threshold, so the user can shrink the context before the
    /// window is full. The transcript itself is never rewritten: only
    /// `compacted_upto` and the compaction record move, exactly like an
    /// automatic pass. Returns immediately with no `ContextCompacted` event
    /// when there is nothing summarizable (fewer than two conversation rounds
    /// after the last compaction).
    pub async fn run_compaction(&self) -> Result<RunOutcome, AgentError> {
        let run_id = new_id();
        let session_id = {
            let mut session = lock_session(&self.session);
            if session.status.is_busy() {
                return Err(AgentError::Busy);
            }
            session.set_status(RunStatus::Running);
            session.steps = 0;
            session.last_error = None;
            session.id.clone()
        };

        self.reset_cancel_token();
        let cancel = self.cancel_token();
        self.emit(HarnessEvent::RunStarted {
            run_id: run_id.clone(),
            session_id,
        });

        let outcome = self.compact_once(&cancel).await;

        {
            let mut session = lock_session(&self.session);
            session.set_status(RunStatus::Idle);
            session.touch();
        }
        self.reset_cancel_token();
        self.emit(HarnessEvent::RunFinished {
            run_id,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    async fn compact_once(&self, cancel: &CancellationToken) -> RunOutcome {
        let system_prompt = self.system_prompt();
        let tool_tokens = context::estimate_tool_specs_tokens(&self.tools.specs());
        match self
            .compact_if_needed(
                &system_prompt,
                tool_tokens,
                crate::session::CompactionTrigger::Manual,
                true,
                cancel,
            )
            .await
        {
            Some(record) => {
                self.emit(HarnessEvent::ContextCompacted { record });
                RunOutcome::Completed
            }
            // Cancelled mid-summary: the host already told the user it stopped.
            None if cancel.is_cancelled() => RunOutcome::Interrupted,
            // Nothing summarizable, or the summary request produced no text and
            // the extractive fallback was empty too. Not an error: the caller
            // reports "nothing to compact".
            None => RunOutcome::Completed,
        }
    }

    async fn run_turn_inner(
        &self,
        input: Option<String>,
        attachments: Vec<UserAttachment>,
    ) -> Result<RunOutcome, AgentError> {
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
                session.push_message(Message::user_with_attachments(input, attachments));
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

        let mut step = 0usize;
        // One reactive compaction per run: if the provider still reports an
        // oversized request after a forced summary, retrying forever would
        // just burn tokens.
        let mut reactive_attempted = false;
        // Output-limit recovery (ZCode's Continue): a provider that ran out of
        // output tokens ends the completion with `FinishReason::Length` but no
        // tool calls, which used to look like a completed turn. Keep asking it
        // to resume instead of leaving a sentence cut in half.
        let mut output_continuation: Option<ChatMessage> = None;
        let mut output_continuations = 0usize;
        // The assistant message a continuation resumes, so its text keeps
        // accumulating in one transcript row instead of splitting the answer.
        let mut continuation_message: Option<String> = None;
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
            let tool_tokens = context::estimate_tool_specs_tokens(&self.tools.specs());

            // Compact before building the request so the estimate is current.
            // This runs at every model step, including steps inside one long
            // tool turn (the case the old user-turn-boundary rule missed).
            if let Some(record) = self
                .compact_if_needed(
                    &system_prompt,
                    tool_tokens,
                    crate::session::CompactionTrigger::Auto,
                    false,
                    cancel,
                )
                .await
            {
                self.emit(HarnessEvent::ContextCompacted { record });
            }
            if cancel.is_cancelled() {
                return RunOutcome::Interrupted;
            }

            let request = {
                let session = lock_session(&self.session);
                let mut messages = context::build_chat_messages(&session, &system_prompt);
                // Appended to the request only (not the transcript), for the
                // rest of this turn, so a resumed answer sees its own partial
                // text and the instruction that follows it.
                if let Some(continuation) = &output_continuation {
                    messages.push(continuation.clone());
                }
                ChatRequest {
                    model: session.model.clone(),
                    messages,
                    tools: self.tools.specs(),
                    temperature: self.config.temperature,
                    max_tokens: self.config.max_tokens,
                    reasoning_effort: self.config.reasoning_effort.clone(),
                }
            };

            let mut stream = StreamState::default();
            if let Some(message_id) = continuation_message.take() {
                stream.message_id = Some(message_id);
                stream.append = true;
            }
            let completion = match self.stream_with_retry(request, &mut stream, cancel).await {
                Ok(Some(completion)) => completion,
                Ok(None) => {
                    self.finish_stream(&mut stream, true);
                    return RunOutcome::Interrupted;
                }
                Err(error)
                    if !reactive_attempted
                        && stream.message_id.is_none()
                        && error.is_context_overflow()
                        && self.config.compaction.enabled =>
                {
                    // The estimate undershot (new tokenizer, ignored tool
                    // schema, provider-side framing). Compact unconditionally
                    // and retry the request once, like ZCode's reactive path.
                    reactive_attempted = true;
                    self.finish_stream(&mut stream, true);
                    let compacted = self
                        .compact_if_needed(
                            &system_prompt,
                            tool_tokens,
                            crate::session::CompactionTrigger::Reactive,
                            true,
                            cancel,
                        )
                        .await;
                    if let Some(record) = compacted {
                        self.emit(HarnessEvent::ContextCompacted { record });
                        continue;
                    }
                    return RunOutcome::Failed(error.to_string());
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

            let (tool_calls, finish_reason) = self.commit_completion(&mut stream, completion);
            if tool_calls.is_empty() {
                // A steer that arrived while the final text was streaming
                // extends the turn: the assistant message above stays in the
                // transcript and the model gets another request with the new
                // user input appended (ZCode's text-only continuation edge).
                if self.has_pending_steers() {
                    output_continuation = None;
                    output_continuations = 0;
                    continue;
                }
                if finish_reason == FinishReason::Length {
                    // The answer hit the output cap. Retry with a transient
                    // "resume" user message (bounded), so a long answer is not
                    // silently presented as complete after being cut mid-word.
                    if output_continuations < MAX_OUTPUT_CONTINUATIONS {
                        output_continuations += 1;
                        output_continuation = Some(ChatMessage::user(OUTPUT_LIMIT_CONTINUE_PROMPT));
                        continuation_message = stream.message_id.clone();
                        continue;
                    }
                    return RunOutcome::Failed(format!(
                        "模型输出达到上限，已自动续写 {MAX_OUTPUT_CONTINUATIONS} 次仍未完成。请在「设置 → 模型」中调高该模型的最大输出，或把任务拆成更小的步骤。"
                    ));
                }
                return RunOutcome::Completed;
            }

            // A tool step is a fresh model call: the resume note has served
            // its purpose and the continuation budget resets.
            output_continuation = None;
            output_continuations = 0;

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

    /// Decide and run one compaction pass.
    ///
    /// The decision uses the same ingredients as ZCode: a percentage-derived
    /// token threshold over the model window, a provider-reported prompt size
    /// when available, a cheap tool-result clearing stage first, and a
    /// model-written summary for everything older than the verbatim tail.
    /// Failures degrade to the extractive fallback instead of killing the turn,
    /// and after `max_consecutive_failures` the harness stops paying for a
    /// summary request that keeps failing.
    async fn compact_if_needed(
        &self,
        system_prompt: &str,
        tool_tokens: usize,
        trigger: crate::session::CompactionTrigger,
        force: bool,
        cancel: &CancellationToken,
    ) -> Option<CompactionRecord> {
        let config = self.config.compaction.clone();
        // `force` is the manual (/compact) and reactive path: an explicit
        // request still compacts when the automatic switch is off.
        if !config.enabled && !force {
            return None;
        }
        let thresholds = config.thresholds(
            self.context_window(),
            self.config.max_tokens.map(|tokens| tokens as usize),
        );

        // `current` is calibrated (local estimate vs. the provider's reported
        // prompt size) and drives the threshold decision. `estimate` keeps the
        // local measure only, so the before/after numbers reported for a
        // compaction compare two identical baselines instead of mixing a
        // provider figure with an estimate.
        let (current, estimate, held) = {
            let mut session = lock_session(&self.session);
            let mut estimate =
                context::estimate_request_tokens(&session, system_prompt, tool_tokens);
            let mut current = estimate.max(session.usage.last_prompt_tokens as usize);

            if config.clear_tool_outputs && (force || current >= thresholds.microcompact_tokens) {
                let outcome = context::microcompact_tool_results(
                    &mut session,
                    config.keep_recent_tool_results,
                );
                if outcome.cleared > 0 {
                    // The provider-reported footprint describes the request
                    // before clearing; recompute from the transcript so the
                    // ring and the next decision see the drop immediately.
                    estimate =
                        context::estimate_request_tokens(&session, system_prompt, tool_tokens);
                    current = estimate;
                    session.usage.last_prompt_tokens = current as u64;
                    session.touch();
                }
            }
            (current, estimate, session.held_compaction.clone())
        };

        let due = force || current >= thresholds.trigger_tokens;
        if due {
            if let Some(held) = held {
                match held.status {
                    crate::session::HeldCompactionStatus::Ready => {
                        if let Some(record) = self.apply_held_compaction(
                            held,
                            trigger,
                            current,
                            system_prompt,
                            tool_tokens,
                            &thresholds,
                        ) {
                            return Some(record);
                        }
                    }
                    crate::session::HeldCompactionStatus::Writing => {
                        // OpenCode waits for an in-flight summary when the
                        // threshold arrives. Bound the wait so a stalled
                        // provider cannot freeze the turn; after that we fall
                        // back to a normal foreground compaction.
                        if let Some(held) = self
                            .wait_for_held_compaction(Duration::from_secs(15), cancel)
                            .await
                        {
                            if held.is_ready()
                                && let Some(record) = self.apply_held_compaction(
                                    held,
                                    trigger,
                                    current,
                                    system_prompt,
                                    tool_tokens,
                                    &thresholds,
                                )
                            {
                                return Some(record);
                            }
                        }
                        if cancel.is_cancelled() {
                            return None;
                        }
                        self.clear_held_compaction();
                    }
                    crate::session::HeldCompactionStatus::Failed => {
                        self.clear_held_compaction();
                    }
                }
            }
        } else {
            // Below the trigger: start (or keep) a background summary and let
            // the main request proceed immediately.
            self.start_background_compaction(current, &thresholds, cancel);
            return None;
        }

        let (pre_tokens, plan, previous_summary, request, use_model, boundary_id) = {
            let session = lock_session(&self.session);
            let plan = context::plan_compaction(&session, thresholds.keep_tokens)?;
            let previous = session
                .compaction
                .as_ref()
                .map(|record| record.summary.clone());
            let input_budget = thresholds
                .context_window
                .saturating_sub(thresholds.summary_tokens)
                .saturating_sub(1_024);
            let request = context::build_compaction_request(
                &session,
                &plan,
                previous.as_deref(),
                input_budget,
                thresholds.summary_tokens,
            );
            let use_model = self.compact_failures.load(Ordering::Relaxed)
                < config.max_consecutive_failures.max(1);
            // A summary request runs without the session lock. Remember the
            // first kept message so an edit/undo that lands meanwhile wins over
            // a stale boundary instead of corrupting `compacted_upto`.
            let boundary_id = session.messages[plan.boundary].id.clone();
            (estimate, plan, previous, request, use_model, boundary_id)
        };

        let mut model_summary = None;
        if use_model {
            match self.request_compaction_summary(request, cancel).await {
                Ok(completion) => {
                    {
                        let mut session = lock_session(&self.session);
                        session.usage.add_background(&completion.usage);
                    }
                    let formatted = context::format_compaction_summary(&completion.text);
                    if !formatted.trim().is_empty() {
                        model_summary = Some(formatted);
                    }
                }
                Err(error) if error.is_cancelled() => return None,
                Err(_) => {
                    self.compact_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        {
            let session = lock_session(&self.session);
            if session.messages.get(plan.boundary).map(|m| m.id.as_str())
                != Some(boundary_id.as_str())
            {
                return None;
            }
        }

        let (summary, source) = match model_summary {
            Some(summary) => {
                self.compact_failures.store(0, Ordering::Relaxed);
                (summary, crate::session::SummarySource::Model)
            }
            None => {
                let session = lock_session(&self.session);
                let summary = context::build_heuristic_summary(
                    &session,
                    &plan,
                    previous_summary.as_deref(),
                    thresholds.summary_tokens,
                );
                (summary, crate::session::SummarySource::Heuristic)
            }
        };
        if summary.trim().is_empty() {
            return None;
        }

        let record = {
            let mut session = lock_session(&self.session);
            if session.messages.get(plan.boundary).map(|m| m.id.as_str())
                != Some(boundary_id.as_str())
            {
                return None;
            }
            // Snapshot the mutable compaction state so a fold that turns out to
            // save nothing can be rolled back instead of being persisted.
            let previous_compacted_upto = session.compacted_upto;
            let previous_cleared = session.cleared_tool_calls.clone();
            let previous_record = session.compaction.clone();
            let previous_held = session.held_compaction.clone();

            session.held_compaction = None;
            session.compacted_upto = plan.boundary;
            // Cleared ids covered by the summary can never appear again; keep
            // only ids that still live in the verbatim tail.
            let surviving: HashSet<String> = session.messages[plan.boundary..]
                .iter()
                .filter_map(|message| message.tool_result().map(|r| r.call_id.clone()))
                .collect();
            session
                .cleared_tool_calls
                .retain(|call_id| surviving.contains(call_id));
            let mut record = context::compose_compaction_record(
                summary,
                source,
                trigger,
                &plan,
                pre_tokens,
                0,
                thresholds.trigger_tokens,
            );
            session.compaction = Some(record.clone());
            record.post_tokens =
                context::estimate_request_tokens(&session, system_prompt, tool_tokens);

            // A summary that is not meaningfully smaller than the rounds it
            // replaces buys nothing and leaves `compacted_upto` in a place that
            // makes the next pass fold even less. Report it as a no-op and keep
            // the original transcript window so `/compact` can retry later.
            if record.pre_tokens.saturating_sub(record.post_tokens)
                < context::required_compaction_savings(plan.summarized_tokens)
            {
                session.compacted_upto = previous_compacted_upto;
                session.cleared_tool_calls = previous_cleared;
                session.compaction = previous_record;
                session.held_compaction = previous_held;
                return None;
            }

            session.compaction = Some(record.clone());
            session.usage.last_prompt_tokens = record.post_tokens as u64;
            session.touch();
            record
        };
        Some(record)
    }

    /// Swap a pre-generated summary into the session if its snapshot is still
    /// valid. Invalid held summaries are dropped; the caller falls back to a
    /// normal foreground compaction.
    fn apply_held_compaction(
        &self,
        held: HeldCompaction,
        trigger: crate::session::CompactionTrigger,
        current_tokens: usize,
        system_prompt: &str,
        tool_tokens: usize,
        thresholds: &context::CompactionThresholds,
    ) -> Option<CompactionRecord> {
        let mut session = lock_session(&self.session);
        if session
            .held_compaction
            .as_ref()
            .map(|current| current.id.as_str())
            != Some(held.id.as_str())
        {
            return None;
        }
        let growth_limit = thresholds.trigger_tokens * 2 / 5;
        let valid = held.is_ready()
            && session.compacted_upto == held.start
            && session
                .messages
                .get(held.boundary)
                .is_some_and(|message| message.id == held.boundary_id)
            && session.model == held.model
            && session.provider_id.as_ref() == held.provider_id.as_ref()
            && current_tokens.saturating_sub(held.pre_tokens) <= growth_limit;
        let Some(summary) = held.summary.clone().filter(|_| valid) else {
            session.held_compaction = None;
            return None;
        };

        let end = session.messages.len();
        let cleared: HashSet<String> = session.cleared_tool_calls.iter().cloned().collect();
        let summarized_tokens: usize = session.messages
            [held.start.min(end)..held.boundary.min(end)]
            .iter()
            .map(|message| context::estimate_message_tokens_with_cleared(message, &cleared))
            .sum();
        let plan = context::CompactionPlan {
            start: held.start,
            boundary: held.boundary,
            end,
            summarized_messages: held.summarized_messages,
            kept_messages: end.saturating_sub(held.boundary),
            summarized_tokens,
            kept_tokens: 0,
        };
        // Same baseline on both sides of the fold: `current_tokens` is
        // calibrated with the provider figure, so reporting it as "before" and
        // a local estimate as "after" made healthy compactions look like they
        // grew the request.
        let pre_tokens = context::estimate_request_tokens(&session, system_prompt, tool_tokens);
        let previous_compacted_upto = session.compacted_upto;
        let previous_cleared = session.cleared_tool_calls.clone();
        let previous_record = session.compaction.clone();

        session.held_compaction = None;
        session.compacted_upto = held.boundary;
        let surviving: HashSet<String> = session.messages[held.boundary..]
            .iter()
            .filter_map(|message| message.tool_result().map(|result| result.call_id.clone()))
            .collect();
        session
            .cleared_tool_calls
            .retain(|call_id| surviving.contains(call_id));
        let mut record = context::compose_compaction_record(
            summary,
            crate::session::SummarySource::Model,
            trigger,
            &plan,
            pre_tokens,
            0,
            thresholds.trigger_tokens,
        );
        session.compaction = Some(record.clone());
        record.post_tokens = context::estimate_request_tokens(&session, system_prompt, tool_tokens);

        if record.pre_tokens.saturating_sub(record.post_tokens)
            < context::required_compaction_savings(plan.summarized_tokens)
        {
            // Not worth swapping in: restore the window and let the caller
            // fall back to a foreground pass.
            session.compacted_upto = previous_compacted_upto;
            session.cleared_tool_calls = previous_cleared;
            session.compaction = previous_record;
            return None;
        }

        session.compaction = Some(record.clone());
        session.usage.last_prompt_tokens = record.post_tokens as u64;
        session.touch();
        Some(record)
    }

    /// Wait for an in-flight background summary to finish. Bounded so a stalled
    /// provider cannot stall the main request forever.
    async fn wait_for_held_compaction(
        &self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Option<HeldCompaction> {
        let deadline = Instant::now() + timeout;
        loop {
            let held = lock_session(&self.session).held_compaction.clone();
            if held.as_ref().is_some_and(Self::held_is_stale) {
                return None;
            }
            match held {
                Some(held) if held.status != HeldCompactionStatus::Writing => return Some(held),
                None => return None,
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::select! {
                _ = cancel.cancelled() => return None,
                _ = sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    /// A writing entry whose task died without clearing itself (process pause,
    /// provider ignoring cancellation) must not block background compaction
    /// forever.
    fn held_is_stale(held: &HeldCompaction) -> bool {
        held.status == HeldCompactionStatus::Writing
            && chrono::Utc::now()
                .signed_duration_since(held.started_at)
                .num_minutes()
                >= 5
    }

    fn clear_held_compaction(&self) {
        lock_session(&self.session).held_compaction = None;
    }

    /// Start writing the next summary ahead of the threshold. The task owns a
    /// snapshot of the conversation and a clone of the provider; it never
    /// mutates the visible transcript, only the held-summary slot.
    fn start_background_compaction(
        &self,
        current_tokens: usize,
        thresholds: &context::CompactionThresholds,
        cancel: &CancellationToken,
    ) {
        let config = &self.config.compaction;
        if !config.background_precompact || current_tokens < thresholds.background_start_tokens {
            return;
        }
        if self.compact_failures.load(Ordering::Relaxed) >= config.max_consecutive_failures.max(1) {
            return;
        }

        let (held, request) = {
            let mut session = lock_session(&self.session);
            let status = session.held_compaction.as_ref().map(|held| held.status);
            let stale = session
                .held_compaction
                .as_ref()
                .is_some_and(Self::held_is_stale);
            match status {
                Some(HeldCompactionStatus::Writing) if !stale => return,
                Some(HeldCompactionStatus::Ready) => return,
                // Failed or stale writing: retry from a fresh snapshot.
                _ => session.held_compaction = None,
            }
            let Some(plan) = context::plan_compaction(&session, thresholds.keep_tokens) else {
                return;
            };
            let previous = session
                .compaction
                .as_ref()
                .map(|record| record.summary.clone());
            let input_budget = thresholds
                .context_window
                .saturating_sub(thresholds.summary_tokens)
                .saturating_sub(1_024);
            let request = context::build_compaction_request(
                &session,
                &plan,
                previous.as_deref(),
                input_budget,
                thresholds.summary_tokens,
            );
            let held = HeldCompaction {
                id: new_id(),
                status: HeldCompactionStatus::Writing,
                summary: None,
                start: plan.start,
                boundary: plan.boundary,
                boundary_id: session.messages[plan.boundary].id.clone(),
                summarized_messages: plan.summarized_messages,
                kept_messages: plan.kept_messages,
                pre_tokens: current_tokens,
                model: session.model.clone(),
                provider_id: session.provider_id.clone(),
                started_at: chrono::Utc::now(),
            };
            (held, request)
        };

        let id = held.id.clone();
        {
            let mut session = lock_session(&self.session);
            if session.held_compaction.is_some() {
                return;
            }
            session.held_compaction = Some(held);
        }

        let provider = self.provider.clone();
        let shared = self.session.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let result = AgentHarness::request_summary(provider, request, cancel).await;
            let mut session = lock_session(&shared);
            if session
                .held_compaction
                .as_ref()
                .map(|held| held.id.as_str())
                != Some(id.as_str())
            {
                return;
            }
            match result {
                Ok(completion) => {
                    session.usage.add_background(&completion.usage);
                    let summary = context::format_compaction_summary(&completion.text);
                    if let Some(held) = session.held_compaction.as_mut() {
                        if summary.trim().is_empty() {
                            held.status = HeldCompactionStatus::Failed;
                        } else {
                            held.status = HeldCompactionStatus::Ready;
                            held.summary = Some(summary);
                        }
                    }
                }
                Err(error) if error.is_cancelled() => {
                    // The turn was interrupted; do not leave a dead snapshot
                    // blocking the next turn's background pass.
                    session.held_compaction = None;
                }
                Err(_) => {
                    if let Some(held) = session.held_compaction.as_mut() {
                        held.status = HeldCompactionStatus::Failed;
                    }
                }
            }
            session.touch();
        });
    }

    /// Ask the session model for a text-only summary. Deltas are ignored: the
    /// summary is written to the session only after the provider returns a
    /// complete response, so a cancelled compaction never leaves half a
    /// summary in the transcript.
    async fn request_compaction_summary(
        &self,
        request: ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        Self::request_summary(self.provider.clone(), request, cancel.clone()).await
    }

    /// Provider half of the summary request, detached from `&self` so it can
    /// run in a background task after the harness is dropped.
    async fn request_summary(
        provider: Arc<dyn ModelProvider>,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let future = provider.stream(request, cancel.clone(), sender);
        tokio::pin!(future);
        let mut receiver_open = true;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                event = receiver.recv(), if receiver_open => {
                    if event.is_none() {
                        receiver_open = false;
                    }
                }
                result = &mut future => {
                    return result;
                }
            }
        }
    }

    /// Stream one completion, retrying retryable failures with backoff.
    ///
    /// A retry re-issues the whole request, so any partial output the failed
    /// attempt streamed into the transcript is rolled back first. That is what
    /// lets a dropped SSE stream (the common "chat stopped mid-sentence"
    /// failure) recover automatically instead of surfacing an error the user
    /// has to dismiss.
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
                    let can_retry = attempt <= self.config.max_retries && error.is_retryable();
                    if !can_retry {
                        return Err(error);
                    }
                    // Discard the truncated attempt before retrying. Without
                    // this the transcript would keep a half answer (and the UI
                    // a stale reveal buffer) next to the retried one.
                    if stream.message_id.is_some()
                        || !stream.text.is_empty()
                        || !stream.reasoning.is_empty()
                    {
                        self.discard_stream(stream);
                    }
                    self.emit(HarnessEvent::ProviderRetry {
                        attempt,
                        max_retries: self.config.max_retries,
                        reason: error.to_string(),
                    });
                    let delay = self.config.retry_base_delay * 2u32.pow((attempt - 1) as u32);
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(None),
                        _ = sleep(delay) => {}
                    }
                }
            }
        }
    }

    /// Roll back the partially streamed assistant message of a failed attempt.
    ///
    /// Only reasoning/text can be present: tool calls are committed from the
    /// provider's final completion, after the stream succeeded.
    fn discard_stream(&self, stream: &mut StreamState) {
        if let Some(message_id) = stream.message_id.take() {
            {
                let mut session = lock_session(&self.session);
                session.messages.retain(|message| message.id != message_id);
                session.touch();
            }
            self.emit(HarnessEvent::AssistantDiscarded { message_id });
        }
        *stream = StreamState::default();
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
    fn commit_completion(
        &self,
        stream: &mut StreamState,
        completion: Completion,
    ) -> (Vec<ToolCall>, FinishReason) {
        let finish_reason = completion.finish_reason.clone().unwrap_or_else(|| {
            if completion.tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }
        });
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
            return (Vec::new(), finish_reason);
        }

        let message_id = self.ensure_assistant(stream);
        {
            let mut session = lock_session(&self.session);
            if let Some(message) = session.message_by_id(&message_id) {
                if stream.append {
                    // Resumed answer: `apply_provider_event` already appended
                    // the streamed deltas to this same message, so only the
                    // no-delta providers need the authoritative text pushed.
                    if stream.text.is_empty() && !final_text.is_empty() {
                        message.push_text(final_text.clone());
                    }
                    if stream.reasoning.is_empty() && !final_reasoning.is_empty() {
                        message.push_reasoning(final_reasoning.clone());
                    }
                } else {
                    if !final_text.is_empty() {
                        message.set_text(final_text.clone());
                    }
                    if !final_reasoning.is_empty() {
                        message.set_reasoning(final_reasoning.clone());
                    }
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
        (completion.tool_calls, finish_reason)
    }

    fn finish_stream(&self, stream: &mut StreamState, interrupted: bool) {
        let Some(message_id) = &stream.message_id else {
            return;
        };
        if interrupted {
            let mut session = lock_session(&self.session);
            if let Some(message) = session.message_by_id(message_id) {
                message.interrupted = true;
                message.touch();
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

        // `permission_mode` is resolved through the config so a child harness
        // follows the owning conversation's mode (see `HarnessConfig`).
        let permission_mode = self.config.effective_permission_mode(&self.session);
        // Tools that declare a finer key (device tools group by action) cache
        // "allow for session" per action; everything else caches per tool.
        let approval_key = match tool.approval_key(&call.arguments) {
            Some(suffix) => format!("{}:{suffix}", call.name),
            None => call.name.clone(),
        };
        let already_approved = self
            .approved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&approval_key);
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
            let request = ApprovalRequest::new(call, tool.summarize(&call.arguments))
                .with_risk(tool.risk(&call.arguments));
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
                        .insert(approval_key.clone());
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
            additional_workspaces: self.config.additional_workspaces.clone(),
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
    use crate::provider::{ProviderEvent, ToolSpec, send_event};
    use crate::session::{SessionState, new_shared_session};
    use crate::testing::ScriptedProvider;
    use crate::tool::{AutoApprovalGate, Tool, ToolOutput};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Fails the first request after streaming a partial reasoning block (the
    /// shape of a dropped SSE connection), then answers normally. Used to pin
    /// the automatic retry + rollback behavior.
    struct DropFirstStreamProvider {
        attempts: AtomicUsize,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl Default for DropFirstStreamProvider {
        fn default() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelProvider for DropFirstStreamProvider {
        fn name(&self) -> &str {
            "drop-first-stream"
        }

        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
            events: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
        ) -> Result<Completion, ProviderError> {
            self.requests.lock().unwrap().push(request);
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                send_event(
                    &events,
                    ProviderEvent::ReasoningDelta("half a thought".to_string()),
                )?;
                return Err(ProviderError::Network(
                    "stream ended before the response completed".to_string(),
                ));
            }
            send_event(
                &events,
                ProviderEvent::TextDelta("recovered answer".to_string()),
            )?;
            Ok(Completion {
                text: "recovered answer".to_string(),
                finish_reason: Some(FinishReason::Stop),
                ..Completion::default()
            })
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

    /// Produces enough output to cross the trigger in one step, and yields so a
    /// background summary can finish while the tool is running.
    struct SizedOutputTool {
        name: &'static str,
        bytes: usize,
    }

    #[async_trait]
    impl Tool for SizedOutputTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Return a large observation"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(ToolOutput::text("B".repeat(self.bytes)))
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

    /// Simulate an in-progress turn: one user message followed by many
    /// assistant/tool rounds, exactly the shape that used to defeat the
    /// user-turn-boundary compaction rule.
    fn seed_long_turn(harness: &AgentHarness, rounds: usize, body: &str) {
        let shared = harness.session();
        let mut session = lock_session(&shared);
        session.push_message(Message::user("inspect everything"));
        for index in 0..rounds {
            let mut assistant = Message::assistant();
            let call = ToolCall::new("read_file", json!({"path": format!("src/{index}.rs")}));
            assistant.push_tool_call(call.clone());
            session.push_message(assistant);
            session.push_message(Message::from_tool_result(ToolResult::success(&call, body)));
        }
    }

    fn compacting_config() -> HarnessConfig {
        HarnessConfig {
            context_window_override: Some(4_000),
            compaction: crate::context::CompactionConfig {
                trigger_percent: 60.0,
                keep_recent_percent: 10.0,
                summary_percent: 10.0,
                clear_tool_outputs: false,
                ..crate::context::CompactionConfig::default()
            },
            ..HarnessConfig::default()
        }
    }

    /// First main request overflows, then the summary request succeeds, then
    /// the retried main request succeeds.
    struct OverflowThenTextProvider {
        calls: AtomicUsize,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl ModelProvider for OverflowThenTextProvider {
        fn name(&self) -> &str {
            "overflow-then-text"
        }

        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
            _events: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
        ) -> Result<Completion, ProviderError> {
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(ProviderError::Http {
                    status: 400,
                    body: "This model's maximum context length is 4096 tokens".to_string(),
                }),
                1 => Ok(Completion::text(
                    "<analysis>x</analysis><summary>reactive summary</summary>",
                )),
                _ => Ok(Completion::text("recovered")),
            }
        }
    }

    struct FailSummaryProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for FailSummaryProvider {
        fn name(&self) -> &str {
            "fail-summary"
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
            _events: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
        ) -> Result<Completion, ProviderError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(ProviderError::Other("summary endpoint down".to_string())),
                _ => Ok(Completion::text("continued")),
            }
        }
    }

    /// Separates summary requests from main requests and records whether the
    /// two overlapped. The first main request stays open long enough for the
    /// background summary to finish while it is still streaming.
    struct BackgroundCompactionProvider {
        tool_name: &'static str,
        summary_calls: AtomicUsize,
        main_calls: AtomicUsize,
        main_in_flight: AtomicUsize,
        overlapped: AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for BackgroundCompactionProvider {
        fn name(&self) -> &str {
            "background-compaction"
        }

        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
            _events: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
        ) -> Result<Completion, ProviderError> {
            let is_summary = request.tools.is_empty()
                && request
                    .messages
                    .first()
                    .is_some_and(|message| message.content.contains("TEXT ONLY"));
            if is_summary {
                self.summary_calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                if self.main_in_flight.load(Ordering::SeqCst) > 0 {
                    self.overlapped.store(1, Ordering::SeqCst);
                }
                return Ok(Completion::text(
                    "<analysis>x</analysis><summary>held summary</summary>",
                ));
            }

            let call = self.main_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.main_in_flight.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.main_in_flight.fetch_sub(1, Ordering::SeqCst);
            if call == 1 {
                Ok(Completion::default().with_tool_call(ToolCall::new(self.tool_name, json!({}))))
            } else {
                Ok(Completion::text("done"))
            }
        }
    }

    fn seed_until_tokens(harness: &AgentHarness, target: usize, body: &str) {
        let system_prompt = harness.system_prompt();
        let tool_tokens = context::estimate_tool_specs_tokens(&harness.tools.specs());
        let shared = harness.session();
        let mut session = lock_session(&shared);
        session.push_message(Message::user("inspect everything"));
        for index in 0..40 {
            let mut assistant = Message::assistant();
            let call = ToolCall::new("read_file", json!({"path": format!("src/{index}.rs")}));
            assistant.push_tool_call(call.clone());
            session.push_message(assistant);
            session.push_message(Message::from_tool_result(ToolResult::success(&call, body)));
            if context::estimate_request_tokens(&session, &system_prompt, tool_tokens) >= target {
                break;
            }
        }
    }

    #[tokio::test]
    async fn background_summary_overlaps_the_main_request_and_is_swapped_in() {
        let provider = Arc::new(BackgroundCompactionProvider {
            tool_name: "big_output",
            summary_calls: AtomicUsize::new(0),
            main_calls: AtomicUsize::new(0),
            main_in_flight: AtomicUsize::new(0),
            overlapped: AtomicUsize::new(0),
        });
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let mut tools = ToolRegistry::new();
        tools.register(SizedOutputTool {
            name: "big_output",
            bytes: 6_000,
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider.clone(),
            Arc::new(tools),
            Arc::new(AutoApprovalGate),
            HarnessConfig {
                context_window_override: Some(10_000),
                compaction: crate::context::CompactionConfig {
                    trigger_percent: 60.0,
                    keep_recent_percent: 10.0,
                    summary_percent: 10.0,
                    clear_tool_outputs: false,
                    background_precompact: true,
                    background_start_percent: 40.0,
                    ..crate::context::CompactionConfig::default()
                },
                ..HarnessConfig::default()
            },
            tx,
        );
        // Start below the trigger but above the background line.
        seed_until_tokens(&harness, 4_500, &"s".repeat(1_200));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(provider.summary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.main_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            provider.overlapped.load(Ordering::SeqCst),
            1,
            "summary must run while the main request is still in flight"
        );

        let session = lock_session(&session);
        let record = session.compaction.as_ref().expect("held summary applied");
        assert_eq!(record.summary_source, crate::session::SummarySource::Model);
        assert!(record.summary.contains("held summary"));
        assert!(session.held_compaction.is_none());
        assert!(session.compacted_upto > 0);
    }

    #[tokio::test]
    async fn stale_background_summary_is_discarded_and_recomputed() {
        let provider = Arc::new(BackgroundCompactionProvider {
            tool_name: "huge_output",
            summary_calls: AtomicUsize::new(0),
            main_calls: AtomicUsize::new(0),
            main_in_flight: AtomicUsize::new(0),
            overlapped: AtomicUsize::new(0),
        });
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let mut tools = ToolRegistry::new();
        tools.register(SizedOutputTool {
            name: "huge_output",
            bytes: 20_000,
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider.clone(),
            Arc::new(tools),
            Arc::new(AutoApprovalGate),
            HarnessConfig {
                context_window_override: Some(10_000),
                compaction: crate::context::CompactionConfig {
                    trigger_percent: 60.0,
                    keep_recent_percent: 10.0,
                    summary_percent: 10.0,
                    clear_tool_outputs: false,
                    background_precompact: true,
                    background_start_percent: 40.0,
                    ..crate::context::CompactionConfig::default()
                },
                ..HarnessConfig::default()
            },
            tx,
        );
        seed_until_tokens(&harness, 4_500, &"s".repeat(1_200));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            provider.summary_calls.load(Ordering::SeqCst),
            2,
            "the held summary must be discarded and regenerated"
        );
        let session = lock_session(&session);
        let record = session.compaction.as_ref().expect("foreground compaction");
        assert_eq!(record.summary_source, crate::session::SummarySource::Model);
        assert!(session.held_compaction.is_none());
    }

    #[tokio::test]
    async fn auto_compaction_runs_inside_one_long_turn() {
        let (harness, handle, mut events, provider) = harness_with(
            vec![
                Completion::text("<analysis>x</analysis><summary>model summary</summary>"),
                Completion::text("finished"),
            ],
            default_tools(),
            compacting_config(),
        );
        seed_long_turn(&harness, 6, &"y".repeat(2_000));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1,
            "the seeded transcript must still be a single turn"
        );
        let record = session.compaction.as_ref().expect("compaction record");
        assert_eq!(record.summary_source, crate::session::SummarySource::Model);
        assert!(record.summary.contains("model summary"));
        assert!(record.trigger == crate::session::CompactionTrigger::Auto);
        assert!(record.compacted_messages > 0);
        assert!(record.post_tokens < record.pre_tokens);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools.is_empty());
        assert!(requests[0].messages[0].content.contains("TEXT ONLY"));
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.content.contains("model summary"))
        );

        let mut saw_compaction = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HarnessEvent::ContextCompacted { .. }) {
                saw_compaction = true;
            }
        }
        assert!(saw_compaction);
    }

    #[tokio::test]
    async fn manual_compaction_ignores_the_threshold_and_records_its_trigger() {
        // A small transcript far below the trigger: `/compact` must still
        // summarize, and the record must say the user asked for it.
        let (harness, handle, mut events, provider) = harness_with(
            vec![Completion::text(
                "<analysis>x</analysis><summary>manual summary</summary>",
            )],
            default_tools(),
            HarnessConfig {
                context_window_override: Some(1_000_000),
                compaction: crate::context::CompactionConfig::default(),
                ..HarnessConfig::default()
            },
        );
        seed_long_turn(&harness, 4, "short output");

        let outcome = harness.run_compaction().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let shared = handle.session();
        let session = lock_session(&shared);
        let record = session
            .compaction
            .as_ref()
            .expect("manual compaction record");
        assert_eq!(record.trigger, crate::session::CompactionTrigger::Manual);
        assert_eq!(record.summary_source, crate::session::SummarySource::Model);
        assert!(record.summary.contains("manual summary"));
        assert!(record.compacted_messages > 0);
        assert_eq!(session.status, RunStatus::Idle);
        drop(session);

        let mut saw_compaction = false;
        let mut saw_finished = false;
        while let Ok(event) = events.try_recv() {
            match event {
                HarnessEvent::ContextCompacted { .. } => saw_compaction = true,
                HarnessEvent::RunFinished { .. } => saw_finished = true,
                _ => {}
            }
        }
        assert!(saw_compaction);
        assert!(saw_finished);
        // Exactly one summary request: the manual path never re-enters the step
        // loop, so no follow-up main request is sent.
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn manual_compaction_reports_nothing_to_do_on_an_empty_transcript() {
        let (harness, handle, mut events, provider) =
            harness_with(Vec::new(), default_tools(), HarnessConfig::default());

        let outcome = harness.run_compaction().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let shared = handle.session();
        let session = lock_session(&shared);
        assert!(session.compaction.is_none());
        assert_eq!(session.status, RunStatus::Idle);
        drop(session);
        assert!(provider.requests().is_empty());
        let mut saw_compaction = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HarnessEvent::ContextCompacted { .. }) {
                saw_compaction = true;
            }
        }
        assert!(!saw_compaction);
    }

    #[tokio::test]
    async fn compaction_that_saves_nothing_leaves_the_transcript_window_alone() {
        // A model summary larger than the rounds it replaces must not be kept:
        // moving `compacted_upto` forward would pay for a summary request, grow
        // the request, and leave the next pass less to fold. `/compact` reports
        // this as "nothing to compact" instead of "减少 0".
        let mut filler = String::from("<analysis>x</analysis><summary>");
        filler.push_str(&"verbose summary sentence ".repeat(40));
        filler.push_str("</summary>");
        let (harness, handle, mut events, _) = harness_with(
            vec![Completion::text(filler)],
            default_tools(),
            // A small window makes the keep budget a fixed share of the small
            // seeded transcript, so one leading round is a real fold.
            HarnessConfig {
                context_window_override: Some(20_000),
                compaction: crate::context::CompactionConfig {
                    trigger_percent: 95.0,
                    keep_recent_percent: 5.0,
                    summary_percent: 10.0,
                    clear_tool_outputs: false,
                    ..crate::context::CompactionConfig::default()
                },
                ..HarnessConfig::default()
            },
        );
        seed_long_turn(&harness, 2, "tiny");

        let outcome = harness.run_compaction().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let shared = handle.session();
        let session = lock_session(&shared);
        assert_eq!(
            session.compacted_upto, 0,
            "a fold with no savings must not advance the visible window"
        );
        assert!(session.compaction.is_none(), "no record for a no-op fold");
        drop(session);

        let mut saw_compaction = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HarnessEvent::ContextCompacted { .. }) {
                saw_compaction = true;
            }
        }
        assert!(
            !saw_compaction,
            "the host reports a no-op, not a compaction"
        );
    }

    #[tokio::test]
    async fn microcompact_avoids_a_model_summary_when_clearing_is_enough() {
        let (harness, handle, _, provider) = harness_with(
            vec![Completion::text("done")],
            default_tools(),
            HarnessConfig {
                context_window_override: Some(4_000),
                compaction: crate::context::CompactionConfig {
                    trigger_percent: 60.0,
                    keep_recent_percent: 10.0,
                    summary_percent: 10.0,
                    clear_tool_outputs: true,
                    keep_recent_tool_results: 1,
                    ..crate::context::CompactionConfig::default()
                },
                ..HarnessConfig::default()
            },
        );
        seed_long_turn(&harness, 6, &"q".repeat(2_000));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let shared = handle.session();
        let session = lock_session(&shared);
        assert!(session.compaction.is_none());
        assert!(session.cleared_tool_calls.len() >= 4);
        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "no summary request should be sent");
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .filter(|message| message.content == context::CLEARED_TOOL_RESULT_PLACEHOLDER)
                .count(),
            session.cleared_tool_calls.len()
        );
    }

    #[tokio::test]
    async fn summary_failure_falls_back_to_an_extractive_summary() {
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            Arc::new(FailSummaryProvider {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(default_tools()),
            Arc::new(AutoApprovalGate),
            compacting_config(),
            tx,
        );
        seed_long_turn(&harness, 6, &"z".repeat(2_000));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        let session = lock_session(&session);
        let record = session.compaction.as_ref().expect("fallback compaction");
        assert_eq!(
            record.summary_source,
            crate::session::SummarySource::Heuristic
        );
        assert!(record.summary.contains("extractive"));
    }

    #[tokio::test]
    async fn provider_overflow_triggers_one_reactive_compaction() {
        let provider = Arc::new(OverflowThenTextProvider {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider.clone(),
            Arc::new(default_tools()),
            Arc::new(AutoApprovalGate),
            HarnessConfig {
                context_window_override: Some(20_000),
                compaction: crate::context::CompactionConfig {
                    // High enough that the estimate does not compact on its own.
                    trigger_percent: 95.0,
                    keep_recent_percent: 10.0,
                    summary_percent: 10.0,
                    clear_tool_outputs: false,
                    ..crate::context::CompactionConfig::default()
                },
                ..HarnessConfig::default()
            },
            tx,
        );
        seed_long_turn(&harness, 4, &"w".repeat(1_000));

        let outcome = harness.run_pending().await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        let session = lock_session(&session);
        let record = session.compaction.as_ref().expect("reactive compaction");
        assert_eq!(record.trigger, crate::session::CompactionTrigger::Reactive);
        assert!(record.summary.contains("reactive summary"));
        assert_eq!(session.messages.last().unwrap().text(), "recovered");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
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
    async fn attachment_only_turn_reaches_the_wire_and_titles_the_session() {
        let (harness, handle, _, provider) = harness_with(
            vec![Completion::text("got it")],
            default_tools(),
            HarnessConfig::default(),
        );
        let attachment = UserAttachment {
            name: "notes.md".to_string(),
            path: "notes.md".to_string(),
            kind: crate::message::AttachmentKind::File,
            mime_type: "text/markdown".to_string(),
            size: 5,
            text: Some("hello".to_string()),
            image: None,
            truncated: false,
        };
        let outcome = harness
            .run_turn_with_attachments("", vec![attachment])
            .await
            .unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        {
            let shared = handle.session();
            let session = lock_session(&shared);
            assert_eq!(session.messages.len(), 2);
            assert_eq!(session.messages[0].attachments.len(), 1);
            assert_eq!(session.title, "附件 · notes.md");
        }
        let requests = provider.requests();
        let user = requests[0]
            .messages
            .iter()
            .find(|message| message.role == "user")
            .expect("user message");
        assert!(user.content.contains("<file name=\"notes.md\""));
        assert!(user.content.contains("hello"));
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

    /// A device-style tool: one name, many capabilities, approval grouped by
    /// the concrete action.
    struct ActionTool {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for ActionTool {
        fn name(&self) -> &str {
            "browser"
        }
        fn description(&self) -> &str {
            "Drive a browser"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn approval(&self) -> ApprovalPolicy {
            ApprovalPolicy::Ask
        }
        fn approval_key(&self, arguments: &Value) -> Option<String> {
            arguments
                .get("action")
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            arguments: Value,
        ) -> Result<ToolOutput, ToolError> {
            let action = arguments
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            self.calls.lock().unwrap().push(action.clone());
            Ok(ToolOutput::text(action))
        }
    }

    /// Gate that remembers every approval for the session and records what it
    /// was asked about.
    struct RecordingSessionGate {
        asks: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ApprovalGate for RecordingSessionGate {
        async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
            let action = request
                .arguments
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            self.asks.lock().unwrap().push(action);
            ApprovalDecision::ApproveForSession
        }
    }

    /// Regression: a loop of identical device actions must ask once, and a
    /// different action must ask on its own — the browser tool's `eval` loop
    /// used to re-prompt 40 times (or, with a tool-level cache, to approve
    /// everything at once).
    #[tokio::test]
    async fn session_approvals_are_grouped_by_action() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(ActionTool {
            calls: calls.clone(),
        });
        let asks = Arc::new(Mutex::new(Vec::new()));
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let provider = Arc::new(ScriptedProvider::new(vec![
            Completion::default()
                .with_tool_call(ToolCall::new("browser", json!({"action": "eval"})))
                .with_tool_call(ToolCall::new("browser", json!({"action": "eval"})))
                .with_tool_call(ToolCall::new("browser", json!({"action": "navigate"}))),
            Completion::text("done"),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session,
            provider,
            Arc::new(tools),
            Arc::new(RecordingSessionGate { asks: asks.clone() }),
            HarnessConfig::default(),
            tx,
        );
        let outcome = harness.run_turn("drive the browser").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(&*calls.lock().unwrap(), &["eval", "eval", "navigate"]);
        assert_eq!(
            &*asks.lock().unwrap(),
            &["eval", "navigate"],
            "the second eval must reuse the session approval for that action"
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

    /// Regression: a child harness resolves its permission mode through
    /// `HarnessConfig::permission_mode`, not its own fresh session. Before this,
    /// subagent harnesses ran on a bare broker gate, so FullAccess on the main
    /// conversation still popped an approval card for every child edit.
    #[tokio::test]
    async fn config_permission_mode_provider_overrides_the_session_field() {
        struct AskEditTool {
            calls: Arc<Mutex<Vec<Value>>>,
        }

        #[async_trait]
        impl Tool for AskEditTool {
            fn name(&self) -> &str {
                "write_file"
            }
            fn description(&self) -> &str {
                "Write a file"
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
                arguments: Value,
            ) -> Result<ToolOutput, ToolError> {
                self.calls.lock().unwrap().push(arguments);
                Ok(ToolOutput::text("written"))
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(AskEditTool {
            calls: calls.clone(),
        });
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        // The child's own session stays on the default mode; only the closure
        // that the host installs decides.
        assert_eq!(
            lock_session(&session).permission_mode,
            PermissionMode::Default
        );
        let mode = Arc::new(Mutex::new(PermissionMode::FullAccess));
        let mode_for_gate = mode.clone();
        let gate = Arc::new(crate::tool::ModeApprovalGate::new(
            Arc::new(crate::tool::DenyApprovalGate),
            Arc::new(move || *mode_for_gate.lock().unwrap()),
        ));
        let provider = Arc::new(ScriptedProvider::new(vec![
            Completion::default().with_tool_call(ToolCall::new("write_file", json!({"path": "a"}))),
            Completion::text("done"),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session.clone(),
            provider,
            Arc::new(tools),
            gate,
            HarnessConfig {
                permission_mode: Some(Arc::new(move || *mode.lock().unwrap())),
                ..HarnessConfig::default()
            },
            tx,
        );
        let outcome = harness.run_turn("write it").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "FullAccess must auto-approve child edits"
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
        let steer_id = handle.steer_with_attachments(
            "mid-run",
            vec![UserAttachment {
                name: "shot.txt".to_string(),
                path: "shot.txt".to_string(),
                kind: crate::message::AttachmentKind::File,
                mime_type: "text/plain".to_string(),
                size: 3,
                text: Some("abc".to_string()),
                image: None,
                truncated: false,
            }],
        );
        release.notify_one();
        let outcome = run.await.unwrap().unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        {
            let shared = handle.session();
            let session = lock_session(&shared);
            assert_eq!(session.messages.len(), 5);
            assert!(session.messages[3].mid_turn, "steer is flagged mid_turn");
            assert_eq!(session.messages[3].text(), "mid-run");
            assert_eq!(session.messages[3].attachments.len(), 1);
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
        assert!(second.contains("<file name=\"shot.txt\""));

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

    /// A completion that stops because it ran out of output tokens is not a
    /// finished turn: resume it with a request-only continuation instruction
    /// (ZCode's Continue) instead of showing a sentence cut in half as done.
    #[tokio::test]
    async fn output_limit_continues_the_answer_instead_of_ending_the_turn() {
        let length = || Completion {
            text: "part one".to_string(),
            finish_reason: Some(FinishReason::Length),
            ..Completion::default()
        };
        let (harness, handle, _events, provider) = harness_with(
            vec![
                length(),
                Completion {
                    text: " part two".to_string(),
                    finish_reason: Some(FinishReason::Stop),
                    ..Completion::default()
                },
            ],
            default_tools(),
            HarnessConfig::default(),
        );

        let outcome = harness.run_turn("write a long answer").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "one continuation request");
        // The resume instruction rides at the end of the second request only.
        let last = requests[1].messages.last().expect("continuation message");
        assert_eq!(last.role, "user");
        assert!(last.content.contains("Resume directly"));

        let shared = handle.session();
        let session = lock_session(&shared);
        let assistant_texts: Vec<String> = session
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| message.text())
            .collect();
        // The resumed completion appends to the same message: a turn merges
        // consecutive assistant messages and would only render the last row's
        // text, hiding the first half of the answer.
        assert_eq!(assistant_texts, vec!["part one part two"]);
        // The continuation prompt is transient: the transcript still has the
        // single real user turn and no synthetic resume message.
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1
        );
    }

    /// After the continuation budget is spent, the output limit is reported as
    /// an error instead of silently ending (which looked like a normal stop).
    #[tokio::test]
    async fn output_limit_reports_an_error_after_the_continuation_budget() {
        let script: Vec<Completion> = (0..=MAX_OUTPUT_CONTINUATIONS)
            .map(|index| Completion {
                text: format!("part {index}"),
                finish_reason: Some(FinishReason::Length),
                ..Completion::default()
            })
            .collect();
        let (harness, _handle, _events, provider) =
            harness_with(script, default_tools(), HarnessConfig::default());

        let outcome = harness.run_turn("write forever").await.unwrap();
        match outcome {
            RunOutcome::Failed(message) => assert!(
                message.contains("输出达到上限"),
                "unexpected failure message: {message}"
            ),
            other => panic!("expected a failed run, got {other:?}"),
        }
        assert_eq!(provider.requests().len(), MAX_OUTPUT_CONTINUATIONS + 1);
    }

    /// A dropped stream (partial reasoning, then a network error) is retried
    /// automatically: the half answer is rolled back, the retry runs, and the
    /// user never sees an error card. The host learns about both steps through
    /// `AssistantDiscarded` and `ProviderRetry`.
    #[tokio::test]
    async fn dropped_stream_retries_and_discards_the_partial_answer() {
        let session = new_shared_session(SessionState::new(PathBuf::from("."), "test-model"));
        let provider = Arc::new(DropFirstStreamProvider::default());
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let harness = AgentHarness::new(
            session,
            provider.clone(),
            Arc::new(default_tools()),
            Arc::new(AutoApprovalGate),
            HarnessConfig {
                retry_base_delay: Duration::from_millis(1),
                ..HarnessConfig::default()
            },
            tx,
        );
        let handle = harness.handle();

        let outcome = harness.run_turn("q").await.unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);

        let mut discarded = Vec::new();
        let mut retried = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                HarnessEvent::AssistantDiscarded { message_id } => discarded.push(message_id),
                HarnessEvent::ProviderRetry { .. } => retried += 1,
                _ => {}
            }
        }
        assert_eq!(retried, 1, "one retry announcement");
        assert_eq!(discarded.len(), 1, "the truncated message is rolled back");

        let shared = handle.session();
        let session = lock_session(&shared);
        // Only the successful attempt remains: no second assistant message and
        // no leftover reasoning from the dropped one.
        let assistants: Vec<Message> = session
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .cloned()
            .collect();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].text(), "recovered answer");
        assert!(
            session
                .messages
                .iter()
                .all(|message| !message.id.eq(&discarded[0]))
        );
    }
}
