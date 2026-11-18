//! Agent runtime: run one child, wake it on messages, and expose the address
//! book to tools.
//!
//! This module deliberately knows nothing about the graph UI. It produces
//! plain events ([`AppEvent`]) and descriptors; the canvas is a separate
//! consumer. The registry (names, records, visibility) lives in
//! [`super::registry`], shared types in [`super::types`], and the system
//! prompt in [`super::prompt`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use effibuddy_agent_core::tool::{ModeApprovalGate, PermissionMode};
use effibuddy_agent_core::{
    AgentHarness, AgentMessage, AgentMessageKind, AgentSendOutcome, ApprovalDecision,
    BrokerApprovalGate, ChildRunRecord, ContentBlock, HarnessConfig, HarnessEvent, MAX_AGENT_DEPTH,
    ModelProvider, ResolvedModel, Role, RunOutcome, SessionState, SubagentLaunch, SubagentProfile,
    SubagentReport, SubagentRequest, SubagentSpawner, SubagentStatus, ToolError, ToolRegistry,
    extract_json_value, lock_session, new_shared_session, validate_json,
};
use effibuddy_agent_tools::{BrowserTool, ComputerTool, Workspace, builtin_tools_with_vision};
use effibuddy_browser::BrowserRuntime;
use effibuddy_computer::ComputerRuntime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::events::{AgentMessageDeliveredInfo, AppEvent, SubagentSpawnedInfo};
use crate::store::SessionStore;

use super::prompt::child_prompt;
use super::registry::AgentRegistry;
use super::types::{PreparedChild, can_address, descriptor_for, failed_report, is_mutating_tool};

#[derive(Clone)]
pub struct SubagentService {
    provider: Arc<dyn ModelProvider>,
    parent_session_id: String,
    parent_provider_id: String,
    parent_model: String,
    parent_reasoning: Option<String>,
    base_config: HarnessConfig,
    project_instructions: String,
    store: SessionStore,
    events: UnboundedSender<AppEvent>,
    runtime: tokio::runtime::Handle,
    tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    gates: Arc<Mutex<HashMap<String, Arc<BrokerApprovalGate>>>>,
    registry: AgentRegistry,
    /// Parent workspace root; the project-scope profile directory lives under
    /// `<workspace>/.effibuddy/agents`.
    workspace: std::path::PathBuf,
    /// Shared snapshot of the effective profile catalog. The same Arc is
    /// installed in the harness configs, so refreshed Markdown profiles reach
    /// the next system prompt without rebuilding anything.
    catalog: Arc<std::sync::RwLock<Vec<SubagentProfile>>>,
    /// Diagnostics from the last catalog refresh (invalid Markdown files).
    catalog_diagnostics: Arc<Mutex<Vec<effibuddy_agent_core::ProfileDiagnostic>>>,
    /// Wall clock of the last profile-directory scan. Spawning fan-outs call
    /// [`Self::refresh_catalog`] per launch; scanning the profile directories
    /// on real disk each time collapses spawn throughput at 1000 children, so
    /// refreshes are TTL-coalesced (edits still land within the window).
    catalog_refreshed_at: Arc<Mutex<Option<Instant>>>,
    /// Durable provider/model registry used to resolve profile `model`.
    models: Arc<effibuddy_agent_core::ModelConfigFile>,
    /// Live permission mode of the owning conversation. Every child harness
    /// resolves its mode through this closure, so one Shift+Tab in the composer
    /// governs the whole agent tree instead of leaving children on a bare
    /// broker gate that keeps asking (or keeps denying) in the old mode.
    permission_mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    /// Device tools shared with the main conversation. When present, children
    /// register `computer` / `browser` exactly like the main agent does and the
    /// same permission matrix decides every call.
    devices: Option<DeviceTools>,
}

/// Handles a child needs to register the device tools. The runtimes are the
/// main conversation's, so a child's calls reach the same screen and browser
/// and share the same pause/stop state.
#[derive(Clone)]
pub struct DeviceTools {
    pub computer: Arc<ComputerRuntime>,
    pub browser: Arc<BrowserRuntime>,
    /// Whether the owning conversation's model can read screenshots; per-child
    /// model overrides win over this.
    pub vision: bool,
}

/// Minimum spacing between profile-directory scans per service.
const CATALOG_REFRESH_TTL: Duration = Duration::from_secs(2);

/// Minimum spacing between child-session saves in the event forwarder. The
/// forwarder used to clone + serialize the whole child session on *every*
/// structural event; a 1000-child fan-out turned that into unbounded JSON
/// throughput. Mirrors the main session's persist throttle.
const CHILD_SAVE_MIN_INTERVAL: Duration = Duration::from_millis(800);

impl SubagentService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        parent_session_id: String,
        parent_provider_id: String,
        parent_model: String,
        parent_reasoning: Option<String>,
        base_config: HarnessConfig,
        project_instructions: String,
        store: SessionStore,
        events: UnboundedSender<AppEvent>,
        runtime: tokio::runtime::Handle,
        workspace: std::path::PathBuf,
        models: Arc<effibuddy_agent_core::ModelConfigFile>,
    ) -> Self {
        let mut main =
            effibuddy_agent_core::AgentDescriptor::main(parent_session_id.clone(), "main");
        main.description = "主智能体：与用户对话并调度子代理与智能体组".to_string();
        let registry = AgentRegistry::new(main);
        let user_agents_dir = crate::config::base_dir().join("agents");
        let (catalog, diagnostics) =
            effibuddy_agent_core::merged_subagent_profiles(&user_agents_dir, Some(&workspace));
        let catalog = Arc::new(std::sync::RwLock::new(catalog));
        let catalog_diagnostics = Arc::new(Mutex::new(diagnostics));
        let catalog_refreshed_at = Arc::new(Mutex::new(None));
        let mut base_config = base_config;
        base_config.agent_catalog = catalog.clone();
        // Children inherit the owning conversation's live mode when the host
        // supplied one through `HarnessConfig`; otherwise they run in `Default`
        // mode, which is the behavior tests and the stress lab rely on.
        let permission_mode = base_config.permission_mode.clone().unwrap_or_else(|| {
            Arc::new(|| PermissionMode::Default) as Arc<dyn Fn() -> PermissionMode + Send + Sync>
        });
        Self {
            provider,
            parent_session_id,
            parent_provider_id,
            parent_model,
            parent_reasoning,
            base_config,
            project_instructions,
            store,
            events,
            runtime,
            tokens: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
            registry,
            workspace,
            catalog,
            catalog_diagnostics,
            catalog_refreshed_at,
            models,
            permission_mode,
            devices: None,
        }
    }

    /// Let subagents register the same device tools as the main agent. Without
    /// this they keep a workspace-only tool surface.
    pub fn with_device_tools(mut self, devices: DeviceTools) -> Self {
        self.devices = Some(devices);
        self
    }

    /// Main conversation this service belongs to. Runs from other sessions
    /// stay untouched when the user stops the active one.
    pub fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    /// Make every child harness follow `provider`'s permission mode. Without
    /// this the children resolve the mode from their own fresh session (always
    /// `Default`), which is what tests and the stress lab want.
    pub fn with_permission_mode(
        mut self,
        provider: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        self.permission_mode = provider;
        self
    }

    /// Reload Markdown profiles from both scopes and publish the merged
    /// catalog. Runs on construction, on every spawn and before the catalog
    /// query, so file edits are picked up without a restart.
    fn refresh_catalog(&self) -> Vec<SubagentProfile> {
        // TTL coalescing: fan-out spawns call this per launch; without the
        // floor each spawn re-scans the profile directories on disk.
        if let Ok(fresh_at) = self.catalog_refreshed_at.lock() {
            if let Some(at) = *fresh_at {
                if at.elapsed() < CATALOG_REFRESH_TTL {
                    if let Ok(slot) = self.catalog.read() {
                        if !slot.is_empty() {
                            return slot.clone();
                        }
                    }
                }
            }
        }
        let user_agents_dir = crate::config::base_dir().join("agents");
        let (merged, diagnostics) =
            effibuddy_agent_core::merged_subagent_profiles(&user_agents_dir, Some(&self.workspace));
        if let Ok(mut slot) = self.catalog.write() {
            *slot = merged.clone();
        }
        if let Ok(mut slot) = self.catalog_diagnostics.lock() {
            *slot = diagnostics;
        }
        if let Ok(mut fresh_at) = self.catalog_refreshed_at.lock() {
            *fresh_at = Some(Instant::now());
        }
        merged
    }

    fn emit(&self, event: AppEvent) {
        let _ = self.events.send(event);
    }

    fn prepare(
        &self,
        agent_id: &str,
        request: &SubagentRequest,
    ) -> Result<PreparedChild, ToolError> {
        if request.depth > MAX_AGENT_DEPTH {
            return Err(ToolError::new(format!(
                "agent tree depth limit reached ({MAX_AGENT_DEPTH})"
            )));
        }
        let parent_read_only = self.registry.parent_read_only(request);
        let catalog = self.refresh_catalog();
        let mut profile = catalog
            .iter()
            .find(|candidate| {
                effibuddy_agent_core::normalize_profile_name(&candidate.name)
                    == effibuddy_agent_core::normalize_profile_name(&request.profile)
            })
            .cloned()
            .unwrap_or_else(|| SubagentProfile {
                name: request.profile.clone(),
                description: request.description.clone(),
                prompt: format!(
                    "You are the custom agent `{}`. Complete the task your launcher assigned to you.",
                    request.profile
                ),
                tools: Vec::new(),
                disallowed_tools: Vec::new(),
                model: None,
                read_only: parent_read_only || request.read_only.unwrap_or(false),
                allow_spawn: request.allow_spawn,
                color: effibuddy_agent_core::SubagentColor::Blue,
                source: effibuddy_agent_core::ProfileSource::BuiltIn,
            });
        if let Some(prompt) = &request.system_prompt {
            profile.prompt = prompt.clone();
        }
        // A read-only parent must never create a child that can mutate the
        // workspace; an explicit `read_only: false` cannot escalate.
        let read_only = parent_read_only || profile.read_only || request.read_only.unwrap_or(false);
        profile.read_only = read_only;
        let allow_spawn =
            (request.allow_spawn || profile.allow_spawn) && request.depth < MAX_AGENT_DEPTH;
        let name = self.registry.assign_name(agent_id, request, &profile);

        // Model selection: the launcher's model unless the profile overrides
        // it. An unresolvable override fails the spawn instead of silently
        // falling back, so a profile never runs on the wrong model.
        let mut provider = self.provider.clone();
        let mut provider_id = self.parent_provider_id.clone();
        let mut model = self.parent_model.clone();
        let mut reasoning = self.parent_reasoning.clone();
        // Set when the profile pins its own model: the child then also takes
        // that model's per-model budgets (context window, output tokens).
        let mut model_override: Option<ResolvedModel> = None;
        if !profile.inherits_model() {
            let requested = profile.model.as_deref().unwrap_or_default().trim();
            let (provider_hint, model_id) = match requested.split_once('/') {
                Some((prefix, rest)) if !prefix.is_empty() && !rest.is_empty() => {
                    (Some(prefix), rest)
                }
                _ => (None, requested),
            };
            let resolved = self.models.resolve(provider_hint, Some(model_id), None);
            if resolved.synthesized {
                return Err(ToolError::new(format!(
                    "profile `{}` requests unknown model `{requested}`; add it in 设置 → 模型",
                    profile.name
                )));
            }
            if !resolved.live {
                return Err(ToolError::new(format!(
                    "profile `{}` requests model `{requested}` but provider `{}` has no API key",
                    profile.name,
                    resolved.provider_label()
                )));
            }
            // Children inherit the parent conversation's routing id: the child
            // session is created below, and a stable id keeps gateway cache
            // hits for fan-outs instead of a fresh one per child.
            provider = resolved
                .build_provider_for(Some(&self.parent_session_id))
                .map_err(|error| {
                    ToolError::new(format!("无法为 `{requested}` 创建模型客户端：{error}"))
                })?;
            provider_id = resolved.provider.id.clone();
            model = resolved.model.id.clone();
            reasoning = resolved.reasoning_level.clone();
            model_override = Some(resolved);
        }

        let mut session = SessionState::new(request.workspace.clone(), model.clone());
        session.set_model_selection(provider_id, model, reasoning);
        // Snapshot for the transcript record; the harness itself re-reads the
        // owning conversation's mode on every tool call through
        // `HarnessConfig::permission_mode`.
        session.permission_mode = (self.permission_mode)();
        session.title = format!("{} · {}", name, request.description);
        session.parent_id = Some(request.parent_session_id.clone());
        session.agent_name = Some(name.clone());
        session.agent_description = Some(request.description.clone());
        // Children inherit the parent's project so they can be regrouped if
        // they ever surface outside the agent panel.
        session.project_id = self.base_config.project_id.clone();
        if session.project_id.is_some() {
            session.project = Some(request.workspace.clone());
        }
        let shared = new_shared_session(session.clone());
        // Persist before announcing so the child always exists on disk.
        let _ = self.store.save(&session);

        // Device tools follow the child's own model: a profile that pins a
        // non-vision model must not receive screenshots it cannot read.
        let vision = model_override
            .as_ref()
            .map(|resolved| resolved.model.capabilities.vision)
            .or_else(|| self.devices.as_ref().map(|devices| devices.vision))
            .unwrap_or(false);
        let tools = self.child_tools(request, &profile, allow_spawn, read_only, vision)?;

        let mut config = self.base_config.clone();
        config.system_prompt_extra.clear();
        // The output budget belongs to the model, not the conversation: a
        // profile that overrides `model` must also adopt that model's
        // `max_output_tokens` instead of inheriting the parent's.
        if let Some(overridden) = model_override.as_ref() {
            config.max_tokens = Some(overridden.max_output_tokens());
        }
        config.additional_workspaces = request.additional_workspaces.clone();
        config.system_prompt_override = Some(child_prompt(
            &profile,
            request,
            &name,
            allow_spawn,
            &self.project_instructions,
        ));
        config.auto_approve_tools = false;
        config.agent_id = agent_id.to_string();
        config.agent_name = Some(name.clone());
        config.agent_depth = request.depth;
        // The child's approval decisions follow the owning conversation, not its
        // own fresh session. The gate re-reads the closure per call, so a
        // Shift+Tab mid-run reaches children too.
        config.permission_mode = Some(self.permission_mode.clone());

        Ok(PreparedChild {
            request: request.clone(),
            profile,
            name,
            shared,
            tools: Arc::new(tools),
            config,
            provider,
            allow_spawn,
            read_only,
        })
    }

    fn child_tools(
        &self,
        request: &SubagentRequest,
        profile: &SubagentProfile,
        allow_spawn: bool,
        read_only: bool,
        vision: bool,
    ) -> Result<ToolRegistry, ToolError> {
        let mut registry =
            builtin_tools_with_vision(&request.workspace, &request.additional_workspaces, vision)?;
        // Device tools are ordinary tools: register them for children whenever
        // the capability is on. `filtered()` below then applies the profile
        // allow/deny lists and the read-only contract exactly as for files.
        if let Some(devices) = &self.devices {
            if devices.computer.mode().is_enabled() {
                registry.register(ComputerTool::new(devices.computer.clone(), vision));
            }
            if devices.browser.mode().is_enabled()
                && let Ok(sandbox) = Workspace::with_roots(
                    &request.workspace,
                    &request.additional_workspaces,
                )
            {
                registry.register(BrowserTool::new(
                    devices.browser.clone(),
                    vision,
                    sandbox,
                ));
            }
        }
        let explicit: Option<HashSet<String>> = request
            .tools
            .clone()
            .filter(|tools| !tools.is_empty())
            .map(|tools| tools.into_iter().collect())
            .or_else(|| {
                if profile.tools.is_empty() {
                    None
                } else {
                    Some(profile.tools.iter().cloned().collect())
                }
            });
        registry = registry.filtered(|tool| {
            if tool == "agent" || tool == "workflow_run" {
                return allow_spawn;
            }
            // Messaging is safe: read-only agents may still talk.
            if tool == "agent_send" || tool == "agent_list" {
                return true;
            }
            // Profile blacklist wins over the allow-list, including when the
            // profile inherits the parent's full tool set.
            if profile
                .disallowed_tools
                .iter()
                .any(|name| name == tool || name == "*")
            {
                return false;
            }
            if read_only && is_mutating_tool(tool) {
                return false;
            }
            match &explicit {
                Some(names) => names.contains(tool),
                None => true,
            }
        });
        Ok(registry)
    }

    fn spawned_info(
        &self,
        agent_id: &str,
        prepared: &PreparedChild,
        started_at: Instant,
    ) -> SubagentSpawnedInfo {
        // Lock once: two `lock_session` calls in one struct literal would hold
        // the first temporary guard until the statement ends and self-deadlock.
        let (child_session_id, model) = {
            let session = lock_session(&prepared.shared);
            (session.id.clone(), session.model.clone())
        };
        SubagentSpawnedInfo {
            agent_id: agent_id.to_string(),
            name: prepared.name.clone(),
            profile: prepared.profile.name.clone(),
            color: prepared.profile.color,
            description: prepared.request.description.clone(),
            child_session_id,
            parent_call_id: prepared.request.parent_call_id.clone(),
            parent_session_id: prepared.request.parent_session_id.clone(),
            parent_agent_id: prepared.request.parent_agent_id.clone(),
            parent_agent_name: prepared.request.parent_agent_name.clone(),
            group_id: prepared.request.group_id.clone(),
            group_name: prepared.request.group_name.clone(),
            depth: prepared.request.depth,
            visibility: prepared.request.visibility,
            read_only: prepared.read_only,
            background: prepared.request.background,
            can_spawn: prepared.allow_spawn,
            provider_id: Some(self.parent_provider_id.clone()),
            model,
            started_at,
            shared: prepared.shared.clone(),
            task_prompt: prepared.request.prompt.clone(),
            output_schema: prepared.request.output_schema.clone(),
        }
    }

    async fn execute_run(
        &self,
        prepared: PreparedChild,
        agent_id: String,
        cancel: CancellationToken,
        input: Option<String>,
        wake: bool,
    ) -> SubagentReport {
        let started = Instant::now();
        let (child_tx, mut child_rx) = unbounded_channel::<HarnessEvent>();
        let broker = Arc::new(BrokerApprovalGate::new(
            child_tx.clone(),
            Duration::from_secs(600),
        ));
        {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            gates.insert(agent_id.clone(), broker.clone());
        }
        // The mode gate sits between the child harness and the broker: with
        // FullAccess the child's edits and commands are approved locally (no
        // card), Plan denies them locally, AcceptEdits approves edits. The
        // closure is the owning conversation's, so children never drift from
        // what the composer shows.
        let approval_gate = Arc::new(ModeApprovalGate::new(
            broker.clone() as Arc<dyn effibuddy_agent_core::ApprovalGate>,
            self.permission_mode.clone(),
        ));

        let harness = AgentHarness::new(
            prepared.shared.clone(),
            prepared.provider.clone(),
            prepared.tools.clone(),
            approval_gate,
            prepared.config.clone(),
            child_tx.clone(),
        )
        .with_subagents(Arc::new(self.clone()) as Arc<dyn SubagentSpawner>);
        let handle = harness.handle();

        let events = self.events.clone();
        let forward_shared = prepared.shared.clone();
        let forward_store = self.store.clone();
        let forward_agent = agent_id.clone();
        let forward_parent = self.parent_session_id.clone();
        let last_save = Arc::new(Mutex::new(None::<Instant>));
        let flush_scheduled = Arc::new(AtomicBool::new(false));
        let save_runtime = self.runtime.clone();
        let forward = self.runtime.spawn(async move {
            while let Some(event) = child_rx.recv().await {
                let structural = matches!(
                    &event,
                    HarnessEvent::AssistantFinished { .. }
                        | HarnessEvent::ToolCallFinished { .. }
                        | HarnessEvent::TodosUpdated { .. }
                        | HarnessEvent::ContextCompacted { .. }
                );
                if structural {
                    // Throttle + trailing flush, mirroring the main session's
                    // persist pattern: cloning and serializing a growing child
                    // session on every structural event collapses under
                    // fan-out (1000 children × ~30 structural events/s each).
                    let now = Instant::now();
                    let (due, remaining) = {
                        let mut last = last_save
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let due = last
                            .map(|at| now.duration_since(at) >= CHILD_SAVE_MIN_INTERVAL)
                            .unwrap_or(true);
                        let remaining = last
                            .map(|at| {
                                CHILD_SAVE_MIN_INTERVAL.saturating_sub(now.duration_since(at))
                            })
                            .unwrap_or(Duration::ZERO);
                        if due {
                            *last = Some(now);
                        }
                        (due, remaining)
                    };
                    if due {
                        let snapshot = lock_session(&forward_shared).clone();
                        let _ = forward_store.save(&snapshot);
                    } else if !flush_scheduled.swap(true, Ordering::SeqCst) {
                        let shared = forward_shared.clone();
                        let store = forward_store.clone();
                        let scheduled = flush_scheduled.clone();
                        let last = last_save.clone();
                        save_runtime.spawn(async move {
                            tokio::time::sleep(remaining).await;
                            let snapshot = lock_session(&shared).clone();
                            let _ = store.save(&snapshot);
                            *last.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                Some(Instant::now());
                            scheduled.store(false, Ordering::SeqCst);
                        });
                    }
                }
                if events
                    .send(AppEvent::SubagentHarness {
                        parent_session_id: forward_parent.clone(),
                        agent_id: forward_agent.clone(),
                        event,
                    })
                    .is_err()
                {
                    break;
                }
                #[cfg(feature = "stress")]
                crate::stress::STRESS_PRODUCED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });

        let outcome = {
            let run = async {
                match input {
                    Some(text) => harness.run_turn(text).await,
                    None => harness.run_pending().await,
                }
            };
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result,
                _ = cancel.cancelled() => {
                    handle.interrupt();
                    (&mut run).await
                }
            }
        };

        drop(harness);
        drop(broker);
        drop(child_tx);
        // A child may have driven the desktop or the browser under this
        // session id. Release whatever it parked so the next run starts clean
        // and the main conversation's own state is untouched.
        if let Some(devices) = &self.devices {
            let child_session_id = lock_session(&prepared.shared).id.clone();
            devices
                .computer
                .restore_isolation(&child_session_id)
                .await;
            devices.computer.forget_session(&child_session_id);
            // The browser is shared: a child that never used it must not close
            // it, and one that did only gives up its own tab.
            if devices.browser.session_tab(&child_session_id).is_some() {
                let _ = devices.browser.end_session(&child_session_id).await;
            } else {
                devices.browser.forget(&child_session_id);
            }
        }
        {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            gates.remove(&agent_id);
        }
        {
            let mut tokens = self
                .tokens
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tokens.remove(&agent_id);
        }
        let _ = forward.await;

        let (mut status, mut error) = match &outcome {
            Ok(RunOutcome::Completed) => (SubagentStatus::Completed, None),
            Ok(RunOutcome::Interrupted) => (
                SubagentStatus::Cancelled,
                Some("subagent was interrupted".to_string()),
            ),
            Ok(RunOutcome::Failed(message)) => (SubagentStatus::Failed, Some(message.clone())),
            Err(error) => (SubagentStatus::Failed, Some(error.to_string())),
        };
        if cancel.is_cancelled() {
            status = SubagentStatus::Cancelled;
            error = Some("subagent was stopped".to_string());
        }

        let (report_text, tool_uses, usage, child_session_id) = {
            let session = lock_session(&prepared.shared);
            let report_text = session
                .messages
                .iter()
                .rev()
                .filter(|message| message.role == Role::Assistant)
                .map(|message| message.text())
                .find(|text| !text.trim().is_empty())
                .unwrap_or_default();
            let tool_uses = session
                .messages
                .iter()
                .flat_map(|message| &message.blocks)
                .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
                .count();
            (
                report_text,
                tool_uses,
                session.usage.clone(),
                session.id.clone(),
            )
        };

        // Enforce the output contract before the launcher or a downstream
        // workflow node ever sees the value.
        let mut data = None;
        if status == SubagentStatus::Completed {
            if let Some(schema) = &prepared.request.output_schema {
                match extract_json_value(&report_text) {
                    Some(value) => match validate_json(&value, schema) {
                        Ok(()) => data = Some(value),
                        Err(schema_error) => {
                            status = SubagentStatus::Failed;
                            error = Some(format!(
                                "output did not satisfy output_schema: {schema_error}"
                            ));
                        }
                    },
                    None => {
                        status = SubagentStatus::Failed;
                        error = Some(
                            "output was not valid JSON, but output_schema was required".to_string(),
                        );
                    }
                }
            } else {
                data = extract_json_value(&report_text);
            }
        }

        // The terminal record is the only cross-restart source of "how did
        // this child end"; a restored session without it is Lost.
        {
            let mut session = lock_session(&prepared.shared);
            session.child_run = Some(ChildRunRecord {
                agent_id: agent_id.clone(),
                profile: prepared.profile.name.clone(),
                color: prepared.profile.color,
                status,
                depth: prepared.request.depth,
                ended_at: chrono::Utc::now(),
                usage: session.usage.clone(),
            });
        }
        let snapshot = lock_session(&prepared.shared).clone();
        let _ = self.store.save(&snapshot);

        let report = SubagentReport {
            agent_id: agent_id.clone(),
            name: prepared.name.clone(),
            profile: prepared.profile.name.clone(),
            color: prepared.profile.color,
            description: prepared.request.description.clone(),
            child_session_id,
            parent_call_id: prepared.request.parent_call_id.clone(),
            parent_agent_id: prepared.request.parent_agent_id.clone(),
            group_id: prepared.request.group_id.clone(),
            group_name: prepared.request.group_name.clone(),
            visibility: prepared.request.visibility,
            background: prepared.request.background,
            wake,
            status,
            report: report_text,
            data,
            error,
            tool_uses,
            duration_ms: started.elapsed().as_millis() as u64,
            usage,
        };
        // Update the registry before publishing the terminal event. Listeners
        // (and tests) may immediately message the agent as soon as they observe
        // the finish; the registry must already report it as idle so the
        // message wakes it instead of being queued behind a stale Running state.
        let needs_wake = self.registry.finish(&agent_id, status);
        self.emit(AppEvent::SubagentFinished {
            parent_session_id: self.parent_session_id.clone(),
            report: report.clone(),
        });
        if needs_wake {
            self.spawn_wake(&agent_id);
        }
        report
    }

    fn spawn_wake(&self, agent_id: &str) -> bool {
        let Some((prepared, token)) = self.registry.take_wake(agent_id) else {
            return false;
        };
        {
            let mut tokens = self
                .tokens
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tokens.insert(agent_id.to_string(), token.clone());
        }
        let this = self.clone();
        let wake_agent = agent_id.to_string();
        self.runtime.spawn(async move {
            this.execute_run(prepared, wake_agent, token, None, true)
                .await;
        });
        true
    }

    fn send_message(
        &self,
        from_session: &str,
        to: &str,
        kind: AgentMessageKind,
        text: String,
        data: Option<serde_json::Value>,
    ) -> Result<AgentSendOutcome, ToolError> {
        let from = self
            .registry
            .sender_descriptor(&self.parent_session_id, from_session);
        let (target, target_id) = self.registry.lookup(to).ok_or_else(|| {
            ToolError::new(format!(
                "Unknown agent address `{to}`. Use agent_list to see what you may address."
            ))
        })?;
        if !can_address(&from.id, &target) {
            return Err(ToolError::new(format!(
                "Agent `{}` is private to `{}`. Ask its owner, or create a public agent for shared work.",
                target.name,
                target.owner_name.as_deref().unwrap_or("its creator")
            )));
        }
        if target.kind == effibuddy_agent_core::AgentNodeKind::Group {
            return Err(ToolError::new(format!(
                "`{}` is an agent group, not a messageable agent. Message one of its members.",
                target.name
            )));
        }

        let message = AgentMessage::new(&from, &target, kind, text, data);

        // Main agent: the app owns its transcript and wake policy.
        if target_id.is_none() {
            self.emit(AppEvent::AgentMessageToMain {
                parent_session_id: self.parent_session_id.clone(),
                message,
            });
            return Ok(AgentSendOutcome {
                to: target,
                delivered: true,
                queued: false,
                woke: true,
            });
        }
        let target_id = target_id.expect("checked");
        let (target_session_id, snapshot, running) = self
            .registry
            .append_message(
                &target_id,
                effibuddy_agent_core::Message::from_agent_message(&message),
            )
            .ok_or_else(|| ToolError::new(format!("Unknown agent id `{target_id}`.")))?;
        let _ = self.store.save(&snapshot);
        let woke = if running {
            false
        } else {
            self.spawn_wake(&target_id)
        };
        let outcome = AgentSendOutcome {
            to: target,
            delivered: true,
            queued: !woke,
            woke,
        };
        self.emit(AppEvent::AgentMessageDelivered {
            parent_session_id: self.parent_session_id.clone(),
            info: AgentMessageDeliveredInfo {
                message,
                target_session_id,
                queued: outcome.queued,
                woke: outcome.woke,
            },
        });
        Ok(outcome)
    }

    #[cfg(test)]
    pub(crate) fn record_read_only(&self, agent_id: &str) -> Option<bool> {
        self.registry.record_read_only(agent_id)
    }
}

#[async_trait::async_trait]
impl SubagentSpawner for SubagentService {
    fn profiles(&self) -> Vec<SubagentProfile> {
        self.refresh_catalog()
    }

    async fn run(&self, request: SubagentRequest, cancel: CancellationToken) -> SubagentReport {
        let agent_id = format!("agent_{}", effibuddy_agent_core::ids::short_id());
        let prepared = match self.prepare(&agent_id, &request) {
            Ok(prepared) => prepared,
            Err(error) => {
                return failed_report(&request, agent_id, String::new(), error.message);
            }
        };
        let descriptor = descriptor_for(&agent_id, &prepared);
        let token = CancellationToken::new();
        {
            let mut tokens = self
                .tokens
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tokens.insert(agent_id.clone(), token.clone());
        }
        self.registry
            .register(&agent_id, descriptor, &prepared, token.clone());
        self.emit(AppEvent::SubagentSpawned {
            parent_session_id: self.parent_session_id.clone(),
            info: self.spawned_info(&agent_id, &prepared, Instant::now()),
        });

        // The parent's cancellation must reach the child even when the parent
        // tool future is dropped by the executor.
        let watcher = {
            let parent_cancel = cancel.clone();
            let child_token = token.clone();
            self.runtime.spawn(async move {
                parent_cancel.cancelled().await;
                child_token.cancel();
            })
        };

        let fallback_request = request.clone();
        let fallback_agent = agent_id.clone();
        let child_session_id = lock_session(&prepared.shared).id.clone();
        let this = self.clone();
        let input = prepared.request.prompt.clone();
        let task = self.runtime.spawn(async move {
            this.execute_run(prepared, agent_id, token, Some(input), false)
                .await
        });
        let result = match task.await {
            Ok(report) => report,
            Err(error) => {
                let report = failed_report(
                    &fallback_request,
                    fallback_agent.clone(),
                    child_session_id,
                    format!("subagent task panicked: {error}"),
                );
                self.registry
                    .finish(&fallback_agent, SubagentStatus::Failed);
                self.emit(AppEvent::SubagentFinished {
                    parent_session_id: self.parent_session_id.clone(),
                    report: report.clone(),
                });
                report
            }
        };
        watcher.abort();
        result
    }

    fn launch(&self, request: SubagentRequest) -> Result<SubagentLaunch, ToolError> {
        let agent_id = format!("agent_{}", effibuddy_agent_core::ids::short_id());
        let prepared = self.prepare(&agent_id, &request)?;
        let descriptor = descriptor_for(&agent_id, &prepared);
        let agent_name = prepared.name.clone();
        let child_session_id = lock_session(&prepared.shared).id.clone();
        let token = CancellationToken::new();
        {
            let mut tokens = self
                .tokens
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tokens.insert(agent_id.clone(), token.clone());
        }
        self.registry
            .register(&agent_id, descriptor, &prepared, token.clone());
        self.emit(AppEvent::SubagentSpawned {
            parent_session_id: self.parent_session_id.clone(),
            info: self.spawned_info(&agent_id, &prepared, Instant::now()),
        });

        let this = self.clone();
        let spawn_agent = agent_id.clone();
        let input = prepared.request.prompt.clone();
        self.runtime.spawn(async move {
            let _ = this
                .execute_run(prepared, spawn_agent, token, Some(input), false)
                .await;
        });

        Ok(SubagentLaunch {
            agent_id,
            agent_name,
            child_session_id,
        })
    }

    fn stop(&self, agent_id: &str) {
        let token = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(agent_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
        }
        self.registry.clear_pending(agent_id);
    }

    fn agents(&self) -> Vec<effibuddy_agent_core::AgentDescriptor> {
        self.registry.all()
    }

    fn visible_agents(&self, viewer_agent_id: &str) -> Vec<effibuddy_agent_core::AgentDescriptor> {
        self.registry.visible(viewer_agent_id)
    }

    fn send_agent_message(
        &self,
        from_session: &str,
        to: &str,
        kind: AgentMessageKind,
        text: String,
        data: Option<serde_json::Value>,
    ) -> Result<AgentSendOutcome, ToolError> {
        self.send_message(from_session, to, kind, text, data)
    }

    fn respond_approval(
        &self,
        agent_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> bool {
        let gate = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(agent_id)
            .cloned();
        match gate {
            Some(gate) => gate.respond(request_id, decision),
            None => false,
        }
    }

    fn stop_all(&self) {
        let tokens: Vec<CancellationToken> = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for token in tokens {
            token.cancel();
        }
        self.registry.clear_all_pending();
    }
}
