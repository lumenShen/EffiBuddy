//! In-app stress lab (feature `stress`).
//!
//! Drives N concurrent agent harnesses inside the real application so the
//! foreground event pump, chat state, agent bookkeeping and the renderer are
//! all exercised under load — then shows a live dashboard with CPU/RSS,
//! aggregate tokens/s and event throughput.
//!
//! Isolation guarantees:
//! - compiled out entirely unless `--features stress` is enabled;
//! - every persistence write goes through a RAM-backed (`/dev/shm` tmpfs)
//!   [`SessionStore`] swapped in for the duration and released on exit —
//!   the user's real session files are never touched;
//! - subagents mode rides the production [`SubagentService`] path.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use effibuddy_agent_core::{
    AgentHandle, AgentHarness, AgentVisibility, AutoApprovalGate, HarnessConfig, HarnessEvent,
    RamDir, RunOutcome, SessionState, SharedSession, StressProvider, SubagentRequest,
    SubagentSpawner, SysMonitor, new_shared_session, stress_user_prompt,
};
use effibuddy_agent_tools::{stress_real_tools, stress_tools};
use effibuddy_ui::Theme;
use gpui::{AnyElement, Context, div, prelude::*, px, uniform_list};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::app::AppView;
use crate::events::AppEvent;
use crate::store::SessionStore;
use crate::subagent::SubagentService;

const SAMPLE_STREAMS: usize = 4;

/// Diagnostic: events produced into the pump by the subagent forwarders
/// (vs. consumed by the foreground pump). Consumed events are `StressMetrics.events`.
pub static STRESS_PRODUCED: AtomicU64 = AtomicU64::new(0);
const SAMPLE_TAIL_CHARS: usize = 320;
const TICKER_INTERVAL: Duration = Duration::from_millis(500);
const FINALIZE_DELAY: Duration = Duration::from_millis(1200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StressMode {
    Sessions,
    Subagents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    /// Aggregate dashboard only — sessions-mode events stay on the tokio side.
    Dashboard,
    /// Dashboard plus `SAMPLE_STREAMS` live mini transcripts.
    Sampled,
    /// Extreme: every event is forwarded through the pump and every child is
    /// rendered as a virtualized row.
    Full,
}

#[derive(Clone, Debug)]
pub struct StressLaunch {
    pub mode: StressMode,
    pub n: usize,
    pub rate: f32,
    pub seconds: f64,
    pub chunk_tokens: usize,
    pub seed: u64,
    pub render: RenderMode,
    pub real_tools: bool,
    /// Provider context window (compaction threshold input).
    pub context_window: usize,
    /// Context-fill memory test: each session grows its transcript to a
    /// random fraction of the window within `min..max`.
    pub fill: Option<(f32, f32)>,
}

/// Parse the stress-lab CLI flags: `--stress [N]` plus `--stress-*` options.
pub fn parse_stress_args() -> Option<StressLaunch> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut launch: Option<StressLaunch> = None;
    for (position, flag) in args.iter().enumerate() {
        if flag == "--stress" {
            let n = args
                .get(position + 1)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1000);
            launch = Some(StressLaunch {
                mode: StressMode::Subagents,
                n,
                rate: 500.0,
                seconds: 30.0,
                chunk_tokens: 12,
                seed: 0,
                render: RenderMode::Dashboard,
                real_tools: false,
                context_window: 262_144,
                fill: None,
            });
        }
    }
    let mut launch = launch?;
    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|item| item == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    if let Some(mode) = value("--stress-mode") {
        launch.mode = match mode.as_str() {
            "sessions" => StressMode::Sessions,
            _ => StressMode::Subagents,
        };
    }
    if let Some(rate) = value("--stress-rate").and_then(|v| v.parse().ok()) {
        launch.rate = rate;
    }
    if let Some(seconds) = value("--stress-seconds").and_then(|v| v.parse().ok()) {
        launch.seconds = seconds;
    }
    if let Some(chunk) = value("--stress-chunk").and_then(|v| v.parse().ok()) {
        launch.chunk_tokens = chunk;
    }
    if let Some(seed) = value("--stress-seed").and_then(|v| v.parse().ok()) {
        launch.seed = seed;
    }
    if let Some(render) = value("--stress-render") {
        launch.render = match render.as_str() {
            "full" => RenderMode::Full,
            "sampled" => RenderMode::Sampled,
            _ => RenderMode::Dashboard,
        };
    }
    if args.iter().any(|item| item == "--stress-real-tools") {
        launch.real_tools = true;
    }
    if let Some(window) = value("--stress-context").and_then(|v| v.parse().ok()) {
        launch.context_window = window;
    }
    if let Some(raw) = value("--stress-fill") {
        if let Some((min, max)) = raw.split_once(':') {
            if let (Ok(min), Ok(max)) = (min.parse::<f32>(), max.parse::<f32>()) {
                if (0.0..=1.0).contains(&min) && (0.0..=1.0).contains(&max) && min <= max {
                    launch.fill = Some((min, max));
                }
            }
        }
    }
    Some(launch)
}

// ---------------------------------------------------------------------------
// Metrics (shared with the tokio-side runners)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct StressMetrics {
    pub events: AtomicU64,
    pub tokens: AtomicU64,
    pub tool_calls: AtomicU64,
    pub turns_started: AtomicU64,
    pub turns_completed: AtomicU64,
    pub turns_cancelled: AtomicU64,
    pub turns_failed: AtomicU64,
    pub children_spawned: AtomicU64,
    pub children_finished: AtomicU64,
}

struct WindowSample {
    tok_per_s: f64,
    cpu_percent: f32,
    rss_bytes: u64,
}

/// One sampled live stream rendered as a mini card.
struct SampleStream {
    label: String,
    tail: String,
    tokens: u64,
}

/// One row of the full-render table (`RenderMode::Full`).
#[derive(Clone)]
struct ChildRow {
    label: String,
    status: &'static str,
    tail: String,
}

type Rows = Rc<Vec<ChildRow>>;

/// Where sessions-mode consumer tasks forward events.
#[derive(Clone)]
enum Forward {
    None,
    Sampled(Vec<usize>),
    All,
}

pub struct StressState {
    pub launch: StressLaunch,
    pub metrics: Arc<StressMetrics>,
    pub started: Instant,
    pub deadline: Instant,
    pub finished: bool,
    pub wall_at_finish: Option<f64>,
    pub report: Option<String>,

    stop: CancellationToken,
    session_handles: Vec<AgentHandle>,
    service: Option<SubagentService>,
    ram: Option<Arc<RamDir>>,
    original_store: Option<SessionStore>,
    finalized: bool,

    monitor: SysMonitor,
    samples: Vec<WindowSample>,
    prev_tokens: u64,

    sample_streams: Vec<SampleStream>,
    sampled_indices: Vec<usize>,
    /// Session index → completion-token watermark for the mini cards.
    session_tokens: Mutex<std::collections::HashMap<usize, u64>>,
    /// agent_id → completion-token watermark (subagents mode).
    child_tokens: Mutex<std::collections::HashMap<String, u64>>,
    /// Diagnostic: StepStarted instants, to measure per-step latency.
    step_started_at: Mutex<std::collections::HashMap<String, Instant>>,
    /// First K spawned children, for subagents-mode mini cards.
    child_order: Vec<String>,
    child_tails: Mutex<std::collections::HashMap<String, String>>,
    rows: Rows,
    row_index: Mutex<std::collections::HashMap<String, usize>>,
}

impl StressState {
    fn target_tokens_per_s(&self) -> f64 {
        self.launch.rate as f64 * self.launch.n as f64
    }

    fn wall_secs(&self) -> f64 {
        self.wall_at_finish
            .unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }
}

// ---------------------------------------------------------------------------
// Sessions-mode runner (tokio side)
// ---------------------------------------------------------------------------

/// Spawn one stress session's turn loop. The consumer task always counts
/// metrics; sampled/full render modes additionally forward events into the
/// foreground pump as [`AppEvent::StressDelta`].
#[allow(clippy::too_many_arguments)]
fn start_session_runner(
    index: usize,
    launch: &StressLaunch,
    runtime: &tokio::runtime::Handle,
    provider: Arc<StressProvider>,
    stop: CancellationToken,
    metrics: Arc<StressMetrics>,
    ram: Option<Arc<RamDir>>,
    forward: Forward,
    app_tx: Option<UnboundedSender<AppEvent>>,
) -> anyhow::Result<AgentHandle> {
    let tools: Arc<_> = if launch.real_tools {
        let root = ram
            .as_ref()
            .map(|ram| ram.join(format!("workspace-{index}")))
            .unwrap_or_else(|| std::env::temp_dir().join(format!("effibuddy-stress-ws-{index}")));
        Arc::new(stress_real_tools(root)?)
    } else {
        Arc::new(stress_tools()?)
    };

    let prompt_seed = launch.seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let first_prompt = stress_user_prompt(prompt_seed).to_string();

    let shared: SharedSession = new_shared_session(SessionState::new(
        std::path::PathBuf::from("."),
        "stress-v1",
    ));
    let (tx, mut rx) = unbounded_channel::<HarnessEvent>();
    let config = HarnessConfig {
        auto_approve_tools: true,
        tool_timeout: Duration::from_secs(10),
        retry_base_delay: Duration::from_millis(100),
        max_retries: 1,
        agent_id: format!("stress_{index}"),
        context_window_override: Some(launch.context_window),
        ..HarnessConfig::default()
    };
    let harness = AgentHarness::new(
        shared,
        provider,
        tools,
        Arc::new(AutoApprovalGate),
        config,
        tx,
    );
    let handle = harness.handle();

    // Event consumer: counts metrics; forwards per the render mode.
    // Spawned on the app runtime: `begin_stress` runs on the GPUI thread,
    // where no tokio reactor is ambient.
    {
        let metrics = metrics.clone();
        runtime.spawn(async move {
            let mut last_total: u64 = 0;
            while let Some(event) = rx.recv().await {
                metrics.events.fetch_add(1, Ordering::Relaxed);
                match &event {
                    HarnessEvent::ToolCallStarted { .. } => {
                        metrics.tool_calls.fetch_add(1, Ordering::Relaxed);
                    }
                    HarnessEvent::RunStarted { .. } => {
                        metrics.turns_started.fetch_add(1, Ordering::Relaxed);
                    }
                    HarnessEvent::RunFinished { outcome, .. } => match outcome {
                        RunOutcome::Completed => {
                            metrics.turns_completed.fetch_add(1, Ordering::Relaxed);
                        }
                        RunOutcome::Interrupted => {
                            metrics.turns_cancelled.fetch_add(1, Ordering::Relaxed);
                        }
                        RunOutcome::Failed(_) => {
                            metrics.turns_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    HarnessEvent::UsageUpdated { usage } => {
                        if usage.completion_tokens > last_total {
                            metrics
                                .tokens
                                .fetch_add(usage.completion_tokens - last_total, Ordering::Relaxed);
                            last_total = usage.completion_tokens;
                        }
                    }
                    _ => {}
                }
                let do_forward = match &forward {
                    Forward::None => false,
                    Forward::All => true,
                    Forward::Sampled(set) => set.contains(&index),
                };
                if do_forward {
                    if let Some(app_tx) = &app_tx {
                        let _ = app_tx.send(AppEvent::StressDelta { index, event });
                    }
                }
            }
        });
    }

    // Turn loop.
    {
        let stop = stop.clone();
        runtime.spawn(async move {
            let mut turns: u64 = 0;
            let mut prompt = first_prompt;
            loop {
                if stop.is_cancelled() {
                    break;
                }
                turns += 1;
                match harness.run_turn(prompt.clone()).await {
                    Ok(RunOutcome::Completed) => {}
                    Ok(RunOutcome::Interrupted) => break,
                    Ok(RunOutcome::Failed(error)) => {
                        eprintln!("[stress] session {index}: turn failed: {error}");
                        break;
                    }
                    Err(error) => {
                        eprintln!("[stress] session {index}: turn error: {error}");
                        break;
                    }
                }
                let next_index = prompt_seed ^ turns.wrapping_mul(0x2545_F491_4F6C_DD1D);
                prompt = stress_user_prompt(next_index).to_string();
            }
        });
    }

    Ok(handle)
}

/// The sampled set: `SAMPLE_STREAMS` sessions evenly spaced across the range.
fn sampled_index_set(n: usize) -> Vec<usize> {
    (0..SAMPLE_STREAMS)
        .map(|i| i * n / SAMPLE_STREAMS.max(1))
        .collect()
}

fn forward_policy(launch: &StressLaunch) -> Forward {
    match launch.render {
        RenderMode::Dashboard => Forward::None,
        RenderMode::Sampled => Forward::Sampled(sampled_index_set(launch.n)),
        RenderMode::Full => Forward::All,
    }
}

// ---------------------------------------------------------------------------
// AppView integration
// ---------------------------------------------------------------------------

impl AppView {
    pub(crate) fn begin_stress(&mut self, launch: StressLaunch, cx: &mut Context<Self>) {
        let Some(runtime) = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.handle().clone())
        else {
            self.last_error = Some("无法创建后台运行时；压测不可用。".to_string());
            return;
        };
        let Some(active) = &self.active else {
            return;
        };
        let parent_session_id = session_id(&active.shared);

        // Swap the session store to a RAM-backed one: every write the stress
        // lab makes (child sessions, subagent persistence) lands in tmpfs.
        let ram = match RamDir::acquire("app") {
            Ok(ram) => Arc::new(ram),
            Err(error) => {
                self.last_error = Some(format!("无法创建内存工作区：{error}"));
                return;
            }
        };
        let ram_store = SessionStore::with_directory(ram.join("sessions"));
        let original_store = std::mem::replace(&mut self.store, ram_store.clone());
        let metrics = Arc::new(StressMetrics::default());
        let provider = Arc::new({
            let mut provider = StressProvider::new()
                .with_rate(launch.rate)
                .with_chunk_tokens(launch.chunk_tokens)
                .with_seed(launch.seed ^ 0xABCD_1234)
                .with_context_window(launch.context_window);
            if let Some((min, max)) = launch.fill {
                provider = provider.with_context_fill(min, max);
            }
            provider
        });

        let rows: Vec<ChildRow> = match launch.mode {
            StressMode::Sessions => (0..launch.n)
                .map(|index| ChildRow {
                    label: format!("sess-{index:04}"),
                    status: "运行中",
                    tail: String::new(),
                })
                .collect(),
            StressMode::Subagents => Vec::new(),
        };

        let mut state = StressState {
            stop: CancellationToken::new(),
            session_handles: Vec::new(),
            service: None,
            ram: Some(ram.clone()),
            original_store: Some(original_store),
            finalized: false,
            monitor: SysMonitor::new(),
            samples: Vec::new(),
            prev_tokens: 0,
            sample_streams: sampled_index_set(launch.n)
                .into_iter()
                .map(|index| SampleStream {
                    label: format!("sess-{index:04}"),
                    tail: String::new(),
                    tokens: 0,
                })
                .collect(),
            sampled_indices: sampled_index_set(launch.n),
            session_tokens: Mutex::new(std::collections::HashMap::new()),
            child_tokens: Mutex::new(std::collections::HashMap::new()),
            step_started_at: Mutex::new(std::collections::HashMap::new()),
            child_order: Vec::new(),
            child_tails: Mutex::new(std::collections::HashMap::new()),
            rows: Rc::new(rows),
            row_index: Mutex::new(std::collections::HashMap::new()),
            launch: launch.clone(),
            metrics: metrics.clone(),
            started: Instant::now(),
            deadline: Instant::now() + Duration::from_secs_f64(launch.seconds.max(1.0)),
            finished: false,
            wall_at_finish: None,
            report: None,
        };
        println!(
            "[stress] mode={:?} n={} rate={} tok/s/stream render={:?} ram={}",
            launch.mode,
            launch.n,
            launch.rate,
            launch.render,
            ram.path().display()
        );

        match launch.mode {
            StressMode::Sessions => {
                let forward = forward_policy(&launch);
                let app_tx = if matches!(forward, Forward::Sampled(_) | Forward::All) {
                    Some(self.app_tx.clone())
                } else {
                    None
                };
                for index in 0..launch.n {
                    match start_session_runner(
                        index,
                        &launch,
                        &runtime,
                        provider.clone(),
                        state.stop.clone(),
                        metrics.clone(),
                        Some(ram.clone()),
                        forward.clone(),
                        app_tx.clone(),
                    ) {
                        Ok(handle) => state.session_handles.push(handle),
                        Err(error) => eprintln!("[stress] session {index}: setup failed: {error}"),
                    }
                }
            }
            StressMode::Subagents => {
                // Ride the production path: one SubagentService launching N
                // children; every child event already flows through the
                // foreground pump as AppEvent::SubagentHarness.
                let provider_dyn: Arc<dyn effibuddy_agent_core::ModelProvider> = provider;
                let service = SubagentService::new(
                    provider_dyn,
                    parent_session_id.clone(),
                    "stress".to_string(),
                    "stress-v1".to_string(),
                    None,
                    HarnessConfig {
                        auto_approve_tools: true,
                        tool_timeout: Duration::from_secs(10),
                        context_window_override: Some(launch.context_window),
                        ..HarnessConfig::default()
                    },
                    String::new(),
                    self.store.clone(),
                    self.app_tx.clone(),
                    runtime.clone(),
                    self.config.workspace(),
                    Arc::new(self.models.file().clone()),
                );
                let service_handle = service.clone();
                let stop = state.stop.clone();
                let count = launch.n;
                // Children root their real file tools at the RAM workspace:
                // fast metadata (canonicalize) and zero persistent-disk
                // writes. The real repo is never touched.
                let stress_workspace = ram.join("workspace");
                let _ = std::fs::create_dir_all(stress_workspace.join("src"));
                let _ = std::fs::write(
                    stress_workspace.join("README.md"),
                    "# Virtual workspace\n\nSeeded for the stress lab.\n",
                );
                let _ = std::fs::write(
                    stress_workspace.join("src/lib.rs"),
                    "pub mod agent {\n    pub const VERSION: &str = \"0.1.0\";\n}\n",
                );
                let _ = std::fs::write(
                    stress_workspace.join("docs/plan.md"),
                    "# Plan\n\n- [x] baseline\n- [ ] concurrency\n",
                );
                runtime.spawn(async move {
                    let profiles = effibuddy_agent_core::builtin_subagent_profiles();
                    let profile_name = profiles
                        .first()
                        .map(|profile| profile.name.clone())
                        .unwrap_or_else(|| "scout".to_string());
                    for index in 0..count {
                        if stop.is_cancelled() {
                            break;
                        }
                        let request = SubagentRequest {
                            profile: profile_name.clone(),
                            description: format!("stress child {index}"),
                            prompt: stress_user_prompt(
                                (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                            )
                            .to_string(),
                            background: true,
                            parent_session_id: parent_session_id.clone(),
                            parent_call_id: format!("call_stress_{index}"),
                            workspace: stress_workspace.clone(),
                            agent_name: Some(format!("stress-{index:04}")),
                            parent_agent_id: Some("main".to_string()),
                            parent_agent_name: Some("main".to_string()),
                            depth: 1,
                            system_prompt: None,
                            tools: None,
                            allow_spawn: false,
                            read_only: Some(false),
                            output_schema: None,
                            visibility: AgentVisibility::Private,
                            group_id: None,
                            group_name: None,
                        };
                        let launch_started = std::time::Instant::now();
                        let launch_result = service_handle.launch(request);
                        let launch_elapsed = launch_started.elapsed();
                        if launch_elapsed > Duration::from_millis(5) {
                            eprintln!("[stress] child {index}: slow launch {launch_elapsed:?}");
                        }
                        if let Err(error) = launch_result {
                            eprintln!("[stress] child {index}: launch failed: {error}");
                        }
                        // Yield between batches so the pump keeps up.
                        if index % 16 == 15 {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                });
                state.service = Some(service);
            }
        }

        self.stress = Some(state);
        self.ensure_stress_ticker(cx);
        cx.notify();
    }

    /// Stop the run and schedule finalize (restore store, release RAM).
    pub(crate) fn end_stress(&mut self, cx: &mut Context<Self>) {
        let Some(stress) = self.stress.as_mut() else {
            return;
        };
        if stress.finished {
            return;
        }
        stress.finished = true;
        stress.wall_at_finish = Some(stress.started.elapsed().as_secs_f64());
        stress.stop.cancel();
        for handle in std::mem::take(&mut stress.session_handles) {
            handle.interrupt();
        }
        if let Some(service) = &stress.service {
            service.stop_all();
        }
        println!("[stress] stopping…");
        cx.notify();
    }

    /// After a graceful wind-down: restore the real store, free the RAM
    /// workspace and produce the report.
    pub(crate) fn finalize_stress(&mut self, cx: &mut Context<Self>) {
        let Some(stress) = self.stress.as_mut() else {
            return;
        };
        if stress.finalized {
            return;
        }
        stress.finalized = true;
        if let Some(original) = stress.original_store.take() {
            self.store = original;
        }
        let freed = stress.ram.as_ref().map(|ram| ram.release()).unwrap_or(0);
        stress.report = Some(stress_report(stress, freed));
        println!("[stress] {}", stress.report.as_deref().unwrap_or(""));
        self.refresh_sessions(cx);
        cx.notify();
    }

    /// 500ms dashboard heartbeat: sample CPU/RSS, roll a throughput window and
    /// repaint; also drives the deadline → finalize transition.
    pub(crate) fn ensure_stress_ticker(&mut self, cx: &mut Context<Self>) {
        if self.stress_ticker_active {
            return;
        }
        self.stress_ticker_active = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(TICKER_INTERVAL).await;
                let Ok(keep) = weak.update(cx, |this, cx| {
                    let Some(stress) = this.stress.as_mut() else {
                        return false;
                    };
                    if stress.finished {
                        if !stress.finalized && stress.finalize_due() {
                            this.finalize_stress(cx);
                        }
                        return this.stress.is_some();
                    }
                    stress.tick_window();
                    if Instant::now() >= stress.deadline {
                        this.end_stress(cx);
                    }
                    cx.notify();
                    true
                }) else {
                    break;
                };
                if !keep {
                    break;
                }
            }
            weak.update(cx, |this, _cx| {
                this.stress_ticker_active = false;
            })
            .ok();
        })
        .detach();
    }

    /// Foreground-pump hook: count every event that crosses the pump while a
    /// stress run is active; consume sampled sessions-mode deltas.
    /// Returns `true` when the event was fully consumed by the stress lab.
    pub(crate) fn stress_observe(&mut self, event: &AppEvent) -> bool {
        let Some(stress) = self.stress.as_mut() else {
            return false;
        };
        match event {
            AppEvent::StressDelta { index, event } => {
                stress.observe_session_delta(*index, event);
                true
            }
            AppEvent::SubagentSpawned(info) => {
                stress
                    .metrics
                    .children_spawned
                    .fetch_add(1, Ordering::Relaxed);
                stress.push_child_row(&info.agent_id, &info.name);
                false
            }
            AppEvent::SubagentHarness { agent_id, event } => {
                stress.observe_child_event(agent_id, event);
                false
            }
            AppEvent::SubagentFinished(_) => {
                stress
                    .metrics
                    .children_finished
                    .fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => false,
        }
    }

    /// The dashboard owns the main area while a run is active (or a report is
    /// waiting to be dismissed).
    pub(crate) fn stress_dashboard_active(&self) -> bool {
        self.stress.is_some()
    }

    // --- dashboard ---------------------------------------------------------

    pub(crate) fn render_stress(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(stress) = self.stress.as_ref() else {
            return div().into_any_element();
        };
        let metrics = &stress.metrics;
        let tokens = metrics.tokens.load(Ordering::Relaxed);
        let events = metrics.events.load(Ordering::Relaxed);
        let elapsed = stress.started.elapsed().as_secs_f64();
        let wall = stress.wall_secs();
        let avg_tok = tokens as f64 / wall.max(0.001);
        let avg_events = events as f64 / wall.max(0.001);
        let target = stress.target_tokens_per_s();
        let completed = metrics.turns_completed.load(Ordering::Relaxed);
        let cancelled = metrics.turns_cancelled.load(Ordering::Relaxed);
        let failed = metrics.turns_failed.load(Ordering::Relaxed);
        let spawned = metrics.children_spawned.load(Ordering::Relaxed);
        let children_done = metrics.children_finished.load(Ordering::Relaxed);
        let cpu = stress
            .samples
            .last()
            .map(|sample| sample.cpu_percent)
            .unwrap_or(0.0);
        let rss = stress
            .samples
            .last()
            .map(|sample| sample.rss_bytes)
            .unwrap_or(0);
        let rss_peak = stress
            .samples
            .iter()
            .map(|sample| sample.rss_bytes)
            .max()
            .unwrap_or(0);
        let fps = crate::perf::hud_snapshot()
            .map(|hud| hud.fps)
            .unwrap_or(0.0);

        let stat = |label: &str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w(px(160.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_family(theme.mono_font())
                        .text_color(theme.text)
                        .child(value),
                )
        };

        let mut grid = div().flex().flex_wrap().gap_4();
        grid = grid.child(stat("并发路数", format!("{}", stress.launch.n)));
        grid = grid.child(stat(
            "实测聚合 tok/s",
            format!("{avg_tok:.0} / 目标 {target:.0}"),
        ));
        grid = grid.child(stat("事件吞吐", format!("{avg_events:.0}/s")));
        grid = grid.child(stat("CPU", format!("{cpu:.1}%")));
        grid = grid.child(stat(
            "内存 RSS / 峰值",
            format!("{} / {}", gib(rss as f64), gib(rss_peak as f64)),
        ));
        grid = grid.child(stat(
            "完成轮次",
            format!("{completed}（取消 {cancelled} 失败 {failed}）"),
        ));
        grid = grid.child(stat(
            "子代理 完成/总数",
            format!("{children_done}/{spawned}"),
        ));
        grid = grid.child(stat("帧率", format!("{fps:.0} fps")));
        grid = grid.child(stat(
            "工具调用",
            format!("{}", metrics.tool_calls.load(Ordering::Relaxed)),
        ));
        grid = grid.child(stat("运行时长", format!("{elapsed:.1}s")));

        // Throughput sparkline: one bar per 1s window, height ∝ tok/s.
        let mut spark = div().flex().items_end().gap(px(2.0)).h(px(64.0)).mt_1();
        let peak = stress
            .samples
            .iter()
            .map(|sample| sample.tok_per_s)
            .fold(1.0f64, f64::max);
        for sample in stress.samples.iter().rev().take(90).rev() {
            let height = (sample.tok_per_s / peak * 64.0).clamp(2.0, 64.0) as f32;
            spark = spark.child(
                div()
                    .w(px(6.0))
                    .h(px(height))
                    .rounded(px(1.0))
                    .bg(theme.accent),
            );
        }

        let progress = if stress.finished {
            1.0f32
        } else {
            (elapsed / stress.launch.seconds.max(1.0)).clamp(0.0, 1.0) as f32
        };

        let header_button = if stress.finished {
            div()
                .id("stress-dismiss")
                .px_3()
                .py_1()
                .rounded(px(6.0))
                .bg(theme.surface_alt)
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .child("关闭报告")
                .on_click(cx.listener(|this, _event, _window, _cx| {
                    this.stress = None;
                    this.stress_ticker_active = false;
                }))
        } else {
            div()
                .id("stress-stop")
                .px_3()
                .py_1()
                .rounded(px(6.0))
                .bg(theme.surface_alt)
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .child("结束压测")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.end_stress(cx);
                }))
        };

        let mut root = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .bg(theme.bg)
            .text_color(theme.text)
            .p(px(20.0))
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_size(px(16.0)).child(format!(
                                "压测实验室 · {:?} × {}",
                                stress.launch.mode, stress.launch.n
                            )))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .font_family(theme.mono_font())
                                    .child(format!(
                                        "每路 {} tok/s · {:?} · 内存工作区（零硬盘写入）",
                                        stress.launch.rate, stress.launch.render
                                    )),
                            ),
                    )
                    .child(header_button),
            )
            .child({
                let mut track = div()
                    .h(px(4.0))
                    .w_full()
                    .rounded(px(2.0))
                    .bg(theme.surface_alt)
                    .flex()
                    .overflow_hidden();
                let mut fill = div().h_full().rounded(px(2.0)).bg(theme.accent);
                fill.style().flex_grow = Some(progress);
                let mut spacer = div();
                spacer.style().flex_grow = Some(1.0 - progress);
                track = track.child(fill).child(spacer);
                track
            })
            .child(grid)
            .child(spark);

        if stress.finished {
            if let Some(report) = &stress.report {
                root = root.child(
                    div()
                        .mt_2()
                        .p_3()
                        .rounded(px(8.0))
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.border)
                        .font_family(theme.mono_font())
                        .text_size(px(12.0))
                        .child(report.clone()),
                );
            }
        }

        // Mini live streams.
        if !stress.finished
            && matches!(stress.launch.render, RenderMode::Sampled | RenderMode::Full)
        {
            let mut streams = div().flex().flex_wrap().gap_3();
            match stress.launch.mode {
                StressMode::Sessions => {
                    let cards: Vec<(String, String, u64)> = stress
                        .sample_streams
                        .iter()
                        .map(|stream| (stream.label.clone(), stream.tail.clone(), stream.tokens))
                        .collect();
                    for (label, tail, tokens) in cards {
                        streams = streams.child(mini_stream(theme, &label, &tail, tokens));
                    }
                }
                StressMode::Subagents => {
                    let tails = stress.child_tails.lock().unwrap_or_else(|p| p.into_inner());
                    let tokens = stress
                        .child_tokens
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    for agent_id in stress.child_order.iter().take(SAMPLE_STREAMS) {
                        let tail = tails.get(agent_id).cloned().unwrap_or_default();
                        let token_count = tokens.get(agent_id).copied().unwrap_or(0);
                        streams = streams.child(mini_stream(theme, agent_id, &tail, token_count));
                    }
                }
            }
            root = root.child(streams);
        }

        // Full-render mode: every child as a virtualized row.
        if stress.launch.render == RenderMode::Full && !stress.rows.is_empty() {
            let rows = stress.rows.clone();
            let row_count = rows.len();
            let row_theme = theme.clone();
            root = root.child(
                div()
                    .flex_1()
                    .min_h(px(120.0))
                    .mt_1()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.0))
                    .bg(theme.surface)
                    .child(uniform_list(
                        "stress-child-rows",
                        row_count,
                        move |range, _window, _cx| {
                            rows[range]
                                .iter()
                                .map(|row| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .h(px(24.0))
                                        .px_2()
                                        .border_b_1()
                                        .border_color(row_theme.border)
                                        .child(
                                            div()
                                                .w(px(150.0))
                                                .text_size(px(11.0))
                                                .font_family(row_theme.mono_font())
                                                .text_color(row_theme.text)
                                                .child(row.label.clone()),
                                        )
                                        .child(
                                            div()
                                                .w(px(70.0))
                                                .text_size(px(11.0))
                                                .text_color(row_theme.text_muted)
                                                .child(row.status.to_string()),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .text_size(px(11.0))
                                                .text_color(row_theme.text_faint)
                                                .overflow_hidden()
                                                .child(row.tail.clone()),
                                        )
                                })
                                .collect::<Vec<_>>()
                        },
                    )),
            );
        }

        root.into_any_element()
    }
}

fn mini_stream(theme: &Theme, label: &str, tail: &str, tokens: u64) -> gpui::AnyElement {
    div()
        .w(px(320.0))
        .h(px(140.0))
        .p_2()
        .rounded(px(8.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .font_family(theme.mono_font())
                        .child(format!("{label} · {tokens} tok")),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .line_height(px(15.0))
                        .text_color(theme.text_faint)
                        .child(tail.to_string()),
                ),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// StressState internals
// ---------------------------------------------------------------------------

impl StressState {
    /// Roll one sampling window and record it.
    fn tick_window(&mut self) {
        let tokens = self.metrics.tokens.load(Ordering::Relaxed);
        let tok_per_s = (tokens - self.prev_tokens) as f64;
        self.prev_tokens = tokens;
        let sample = self.monitor.sample();
        self.samples.push(WindowSample {
            tok_per_s,
            cpu_percent: sample.as_ref().map(|s| s.cpu_percent).unwrap_or(0.0),
            rss_bytes: sample.as_ref().map(|s| s.rss_bytes).unwrap_or(0),
        });
        if self.samples.len() > 600 {
            self.samples.remove(0);
        }

        // Refresh mini-card token counters (sessions mode; subagents mode
        // keeps its own watermark map keyed by agent id).
        let session_tokens = self
            .session_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (slot, index) in self.sampled_indices.iter().enumerate() {
            if let Some(tokens) = session_tokens.get(index) {
                self.sample_streams[slot].tokens = *tokens;
            }
        }
    }

    fn finalize_due(&self) -> bool {
        self.started.elapsed() >= FINALIZE_DELAY
    }

    /// Sessions-mode delta arriving through the pump. Metrics were already
    /// counted on the tokio side; this only feeds the visible cards.
    fn observe_session_delta(&mut self, index: usize, event: &HarnessEvent) {
        match event {
            HarnessEvent::TextDelta { delta, .. } => {
                self.append_sample_tail(index, delta);
            }
            HarnessEvent::ReasoningDelta { delta, .. } => {
                self.append_sample_tail(index, delta);
            }
            HarnessEvent::UsageUpdated { usage } => {
                let mut watermarks = self
                    .session_tokens
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let watermark = watermarks.entry(index).or_insert(0);
                if usage.completion_tokens > *watermark {
                    *watermark = usage.completion_tokens;
                }
            }
            HarnessEvent::RunFinished { outcome, .. } => {
                let status = match outcome {
                    RunOutcome::Completed => "完成",
                    RunOutcome::Failed(_) => "失败",
                    RunOutcome::Interrupted => "取消",
                };
                let rows = std::rc::Rc::make_mut(&mut self.rows);
                if let Some(row) = rows.get_mut(index) {
                    row.status = status;
                }
                self.rows = Rc::new(rows.clone());
            }
            _ => {}
        }
    }

    fn append_sample_tail(&mut self, index: usize, delta: &str) {
        let slot = self.sampled_indices.iter().position(|i| *i == index);
        if let Some(slot) = slot {
            let stream = &mut self.sample_streams[slot];
            push_tail(&mut stream.tail, delta);
        }
    }

    /// Subagent-path event arriving through the pump (production routing):
    /// metrics are counted here because there is no tokio-side counter.
    fn observe_child_event(&mut self, agent_id: &str, event: &HarnessEvent) {
        self.metrics.events.fetch_add(1, Ordering::Relaxed);
        match event {
            HarnessEvent::StepStarted { .. } => {
                self.step_started_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(agent_id.to_string(), Instant::now());
            }
            HarnessEvent::ToolCallStarted { .. } => {
                self.metrics.tool_calls.fetch_add(1, Ordering::Relaxed);
                if let Some(started) = self
                    .step_started_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(agent_id)
                {
                    let lag = started.elapsed();
                    if lag > Duration::from_millis(200) {
                        eprintln!(
                            "[stress] step-lag {agent_id}: {lag:?} (StepStarted→ToolCallStarted)"
                        );
                    }
                }
            }
            HarnessEvent::TextDelta { delta, .. } | HarnessEvent::ReasoningDelta { delta, .. } => {
                let mut tails = self
                    .child_tails
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(tail) = tails.get_mut(agent_id) {
                    push_tail(tail, delta);
                }
            }
            HarnessEvent::UsageUpdated { usage } => {
                let mut watermarks = self
                    .child_tokens
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let watermark = watermarks.entry(agent_id.to_string()).or_insert(0);
                if usage.completion_tokens > *watermark {
                    self.metrics
                        .tokens
                        .fetch_add(usage.completion_tokens - *watermark, Ordering::Relaxed);
                    *watermark = usage.completion_tokens;
                }
            }
            HarnessEvent::RunFinished { outcome, .. } => match outcome {
                RunOutcome::Completed => {
                    self.metrics.turns_completed.fetch_add(1, Ordering::Relaxed);
                }
                RunOutcome::Interrupted => {
                    self.metrics.turns_cancelled.fetch_add(1, Ordering::Relaxed);
                }
                RunOutcome::Failed(_) => {
                    self.metrics.turns_failed.fetch_add(1, Ordering::Relaxed);
                }
            },
            _ => {}
        }
    }

    fn push_child_row(&mut self, agent_id: &str, name: &str) {
        {
            let rows = std::rc::Rc::make_mut(&mut self.rows);
            rows.push(ChildRow {
                label: name.to_string(),
                status: "运行中",
                tail: String::new(),
            });
            let index = rows.len() - 1;
            self.row_index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(agent_id.to_string(), index);
            self.rows = Rc::new(rows.clone());
        }
        if self.child_order.len() < SAMPLE_STREAMS {
            self.child_order.push(agent_id.to_string());
        }
        self.child_tails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(agent_id.to_string(), String::new());
    }
}

/// Append a delta to a bounded tail buffer, keeping it char-aligned.
fn push_tail(tail: &mut String, delta: &str) {
    tail.push_str(delta);
    let keep_from = tail.len().saturating_sub(SAMPLE_TAIL_CHARS);
    let cut = tail
        .char_indices()
        .nth(keep_from)
        .map(|(index, _)| index)
        .unwrap_or(0);
    tail.drain(..cut);
}

fn stress_report(stress: &StressState, freed: u64) -> String {
    let wall = stress.wall_secs().max(0.001);
    let tokens = stress.metrics.tokens.load(Ordering::Relaxed);
    let events = stress.metrics.events.load(Ordering::Relaxed);
    let produced = STRESS_PRODUCED.load(Ordering::Relaxed);
    format!(
        "[报告] 聚合 {:.0} tok/s（目标 {:.0}，比率 {:.1}%）· 事件 {:.0}/s（消费 {} / 产出 {}）· 完成 {} 取消 {} 失败 {} · 工具 {} · 内存释放 {}",
        tokens as f64 / wall,
        stress.target_tokens_per_s(),
        100.0 * (tokens as f64 / wall) / stress.target_tokens_per_s().max(1.0),
        events as f64 / wall,
        events,
        produced,
        stress.metrics.turns_completed.load(Ordering::Relaxed),
        stress.metrics.turns_cancelled.load(Ordering::Relaxed),
        stress.metrics.turns_failed.load(Ordering::Relaxed),
        stress.metrics.tool_calls.load(Ordering::Relaxed),
        gib(freed as f64),
    )
}

fn gib(bytes: f64) -> String {
    if bytes >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2}G", bytes / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1}M", bytes / (1024.0 * 1024.0))
    }
}

fn session_id(shared: &SharedSession) -> String {
    effibuddy_agent_core::lock_session(shared).id.clone()
}
