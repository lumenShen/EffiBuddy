//! `effibuddy-stress` — headless concurrency stress lab for the EffiBuddy
//! agent runtime.
//!
//! Drives N concurrent agent harnesses (sessions or subagent-style children)
//! against the virtual [`StressProvider`] at a fixed per-stream token rate
//! (500 tok/s by default), while sampling process CPU/RSS and counting
//! harness events. All persistence goes to a RAM-backed (tmpfs) workspace and
//! is released on exit; the default `sessions` mode never touches a
//! filesystem at all.
//!
//! This binary is a separate crate on purpose: the shipping app never links
//! any of this code. Build it explicitly:
//!
//! ```text
//! cargo run --release -p effibuddy-stress -- --mode sessions --n 1000 --seconds 30
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use effibuddy_agent_core::stress_user_prompt;
use effibuddy_agent_core::{
    AgentHandle, AgentHarness, AutoApprovalGate, HarnessConfig, HarnessEvent, RamDir, RunOutcome,
    SessionState, SharedSession, StressProvider, SysMonitor, lock_session, new_shared_session,
};
use effibuddy_agent_tools::{stress_real_tools, stress_tools};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Sessions,
    Subagents,
}

struct Args {
    mode: Mode,
    n: usize,
    rate: f32,
    seconds: f64,
    chunk_tokens: usize,
    seed: u64,
    workers: usize,
    max_steps: usize,
    max_turns: u64,
    real_tools: bool,
    save_throttle_ms: u64,
    /// Context window used for compaction thresholds and fill targets.
    context_window: usize,
    /// Context-fill mode: each session grows its transcript to a random
    /// fraction of the window within `min..max` (extreme memory test).
    fill: Option<(f32, f32)>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        mode: Mode::Sessions,
        n: 1000,
        rate: 500.0,
        seconds: 30.0,
        chunk_tokens: 12,
        seed: 0,
        workers: std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(4),
        max_steps: 6,
        max_turns: u64::MAX,
        real_tools: false,
        save_throttle_ms: 800,
        context_window: 262_144,
        fill: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || -> Result<String, String> {
            iter.next().ok_or_else(|| format!("{flag}: missing value"))
        };
        match flag.as_str() {
            "--mode" => {
                let raw = value()?;
                args.mode = match raw.as_str() {
                    "sessions" => Mode::Sessions,
                    "subagents" => Mode::Subagents,
                    other => return Err(format!("unknown mode `{other}`")),
                };
            }
            "--n" => args.n = value()?.parse().map_err(|e| format!("--n: {e}"))?,
            "--rate" => args.rate = value()?.parse().map_err(|e| format!("--rate: {e}"))?,
            "--seconds" => {
                args.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?
            }
            "--chunk-tokens" => {
                args.chunk_tokens = value()?
                    .parse()
                    .map_err(|e| format!("--chunk-tokens: {e}"))?
            }
            "--seed" => args.seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
            "--workers" => {
                args.workers = value()?.parse().map_err(|e| format!("--workers: {e}"))?
            }
            "--max-steps" => {
                args.max_steps = value()?.parse().map_err(|e| format!("--max-steps: {e}"))?
            }
            "--max-turns" => {
                args.max_turns = value()?.parse().map_err(|e| format!("--max-turns: {e}"))?
            }
            "--save-throttle-ms" => {
                args.save_throttle_ms = value()?
                    .parse()
                    .map_err(|e| format!("--save-throttle-ms: {e}"))?
            }
            "--real-tools" => args.real_tools = true,
            "--context-window" => {
                args.context_window = value()?
                    .parse()
                    .map_err(|e| format!("--context-window: {e}"))?
            }
            "--fill" => {
                let raw = value()?;
                let (min, max) = raw
                    .split_once(':')
                    .ok_or("--fill expects MIN:MAX, e.g. 0.2:0.7")?;
                let min: f32 = min.parse().map_err(|e| format!("--fill: {e}"))?;
                let max: f32 = max.parse().map_err(|e| format!("--fill: {e}"))?;
                if !(0.0..=1.0).contains(&min) || !(0.0..=1.0).contains(&max) || min > max {
                    return Err("--fill expects 0..1 fractions with MIN <= MAX".into());
                }
                args.fill = Some((min, max));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag `{other}` (try --help)")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "effibuddy-stress — headless concurrency stress lab\n\n\
         USAGE: effibuddy-stress [FLAGS]\n\n\
         FLAGS:\n\
         \x20 --mode sessions|subagents   workload shape (default sessions)\n\
         \x20 --n N                       concurrent streams (default 1000)\n\
         \x20 --rate TOKPS                tokens/s per stream (default 500)\n\
         \x20 --seconds S                 run duration (default 30)\n\
         \x20 --chunk-tokens N            tokens per provider event (default 12)\n\
         \x20 --seed N                    scenario seed (default 0)\n\
         \x20 --workers N                 tokio worker threads (default min(8, cores))\n\
         \x20 --max-steps N               scenario steps per turn (default 6)\n\
         \x20 --max-turns N               stop a session after N turns (default unlimited)\n\
         \x20 --real-tools                use the real file tools over a RAM workspace\n\
         \x20                             (default: virtual tools, pure memory)\n\
         \x20 --save-throttle-ms MS       subagents-mode save throttle (default 800)\n\
         \x20 --context-window N           provider context window (default 262144)\n\
         \x20 --fill MIN:MAX              context-fill memory test: each session grows its\n\
         \x20                             transcript to a random fraction of the window,\n\
         \x20                             e.g. --fill 0.2:0.7 (pair with a high --rate)\n\n\
         All persistence goes to /dev/shm (tmpfs) and is deleted on exit. The\n\
         real sessions directory is never touched."
    );
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Metrics {
    events: AtomicU64,
    text_deltas: AtomicU64,
    reasoning_deltas: AtomicU64,
    tool_calls: AtomicU64,
    completion_tokens: AtomicU64,
    turns_started: AtomicU64,
    turns_completed: AtomicU64,
    turns_cancelled: AtomicU64,
    turns_failed: AtomicU64,
    saves: AtomicU64,
}

struct WindowSample {
    tok_per_s: f64,
    events_per_s: f64,
    cpu_percent: f32,
    rss_bytes: u64,
    active_turns: u64,
}

fn consume_events(
    mut rx: mpsc::UnboundedReceiver<HarnessEvent>,
    metrics: Arc<Metrics>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_total: u64 = 0;
        while let Some(event) = rx.recv().await {
            metrics.events.fetch_add(1, Ordering::Relaxed);
            match event {
                HarnessEvent::TextDelta { .. } => {
                    metrics.text_deltas.fetch_add(1, Ordering::Relaxed);
                }
                HarnessEvent::ReasoningDelta { .. } => {
                    metrics.reasoning_deltas.fetch_add(1, Ordering::Relaxed);
                }
                HarnessEvent::ToolCallStarted { .. } => {
                    metrics.tool_calls.fetch_add(1, Ordering::Relaxed);
                }
                HarnessEvent::UsageUpdated { usage } => {
                    // Track *completion* tokens only — `total_tokens` grows
                    // with every step's prompt estimate, which would inflate
                    // the throughput metric.
                    if usage.completion_tokens > last_total {
                        metrics
                            .completion_tokens
                            .fetch_add(usage.completion_tokens - last_total, Ordering::Relaxed);
                        last_total = usage.completion_tokens;
                    }
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
                _ => {}
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Per-session workload
// ---------------------------------------------------------------------------

/// Build one session harness synchronously and spawn its turn loop.
/// Returns the task handle plus a control handle used to interrupt the
/// in-flight turn when the deadline fires.
fn start_session(
    index: usize,
    args: &Args,
    provider: Arc<StressProvider>,
    stop: CancellationToken,
    metrics: Arc<Metrics>,
    ram: Option<Arc<RamDir>>,
    watermarks: Arc<Mutex<Vec<u64>>>,
) -> anyhow::Result<(tokio::task::JoinHandle<()>, AgentHandle)> {
    // Per-session tool registry: every session gets its own isolated
    // (virtual or RAM-rooted) workspace.
    let tools: Arc<_> = if args.real_tools {
        let root = ram
            .as_ref()
            .map(|ram| ram.join(format!("workspace-{index}")))
            .unwrap_or_else(|| std::env::temp_dir().join(format!("effibuddy-stress-ws-{index}")));
        Arc::new(stress_real_tools(root)?)
    } else {
        Arc::new(stress_tools()?)
    };

    let prompt_seed = args.seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let first_prompt = stress_user_prompt(prompt_seed).to_string();

    let mut session = SessionState::new(PathBuf::from("."), "stress-v1");
    if args.mode == Mode::Subagents {
        session.parent_id = Some("stress-coordinator".to_string());
        session.agent_name = Some(format!("stress-{index}"));
    }
    let shared: SharedSession = new_shared_session(session);

    let (tx, rx) = mpsc::unbounded_channel::<HarnessEvent>();
    let consumer = consume_events(rx, metrics.clone());

    let config = HarnessConfig {
        auto_approve_tools: true,
        tool_timeout: Duration::from_secs(10),
        retry_base_delay: Duration::from_millis(100),
        max_retries: 1,
        agent_id: format!("stress_{index}"),
        agent_depth: if args.mode == Mode::Subagents { 1 } else { 0 },
        context_window_override: Some(args.context_window),
        ..Default::default()
    };
    let harness = AgentHarness::new(
        shared.clone(),
        provider,
        tools,
        Arc::new(AutoApprovalGate),
        config,
        tx,
    );

    let mode = args.mode;
    let max_turns = args.max_turns;
    let save_throttle = Duration::from_millis(args.save_throttle_ms);
    let fill_hold: Option<(f32, usize)> = args.fill.map(|(min, _)| (min, args.context_window));
    let task_metrics = metrics.clone();
    let task_stop = stop;
    let interrupt_handle = harness.handle();
    let task_handle = interrupt_handle.clone();

    let task = tokio::spawn(async move {
        let mut turns: u64 = 0;
        let mut last_save = Instant::now() - save_throttle;
        let mut prompt = first_prompt;
        loop {
            if task_stop.is_cancelled() || turns >= max_turns {
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
            if mode == Mode::Subagents && last_save.elapsed() >= save_throttle {
                last_save = Instant::now();
                task_metrics.saves.fetch_add(1, Ordering::Relaxed);
                save_session(&ram, index, &shared);
            }
            // Context-fill mode: once the watermark is reached, hold turns
            // are tiny — cycling them immediately would clone the (now huge)
            // transcript hundreds of times per second. Pace hold turns.
            if let Some((min_frac, window)) = fill_hold {
                let filled = lock_session(&shared).usage.last_prompt_tokens
                    >= (window as f64 * min_frac as f64 * 0.95) as u64;
                if filled {
                    fill_hold_delay().await;
                }
            }
            let next_index = prompt_seed ^ turns.wrapping_mul(0x2545_F491_4F6C_DD1D);
            prompt = stress_user_prompt(next_index).to_string();
        }

        if mode == Mode::Subagents {
            task_metrics.saves.fetch_add(1, Ordering::Relaxed);
            save_session(&ram, index, &shared);
        }

        let final_prompt = lock_session(&shared).usage.last_prompt_tokens;
        watermarks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(final_prompt);
        task_handle.interrupt();
        drop(harness); // closes the event channel; the consumer drains and exits
        let _ = consumer.await;
    });

    Ok((task, interrupt_handle))
}

/// Persist one child-style session to the RAM store (JSON, atomic rename).
fn save_session(ram: &Option<Arc<RamDir>>, index: usize, shared: &SharedSession) {
    let Some(ram) = ram else { return };
    let session = lock_session(shared);
    let json = match serde_json::to_string_pretty(&*session) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("[stress] session {index}: serialize failed: {error}");
            return;
        }
    };
    drop(session);
    let dir = ram.join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("stress-{index:05}.json"));
    let tmp = dir.join(format!("stress-{index:05}.json.tmp"));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------------------------------------------------------------------------
// Sampler + report
// ---------------------------------------------------------------------------

fn spawn_sampler(
    metrics: Arc<Metrics>,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<Vec<WindowSample>> {
    tokio::spawn(async move {
        let mut monitor = SysMonitor::new();
        let mut samples: Vec<WindowSample> = Vec::new();
        let mut prev_tokens = 0u64;
        let mut prev_events = 0u64;
        let mut tick: u64 = 0;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let tokens = metrics.completion_tokens.load(Ordering::Relaxed);
            let events = metrics.events.load(Ordering::Relaxed);
            let tok_per_s = (tokens - prev_tokens) as f64;
            let events_per_s = (events - prev_events) as f64;
            prev_tokens = tokens;
            prev_events = events;
            let sample = monitor.sample();
            let cpu = sample.map(|s| s.cpu_percent).unwrap_or(0.0);
            let rss = sample.map(|s| s.rss_bytes).unwrap_or(0);
            let active_turns = metrics
                .turns_started
                .load(Ordering::Relaxed)
                .saturating_sub(metrics.turns_completed.load(Ordering::Relaxed))
                .saturating_sub(metrics.turns_cancelled.load(Ordering::Relaxed))
                .saturating_sub(metrics.turns_failed.load(Ordering::Relaxed));
            samples.push(WindowSample {
                tok_per_s,
                events_per_s,
                cpu_percent: cpu,
                rss_bytes: rss,
                active_turns,
            });
            tick += 1;
            if tick % 2 == 0 {
                let rss_mib = rss as f64 / (1024.0 * 1024.0);
                eprintln!(
                    "[stress] tok/s {:>9} events/s {:>8} cpu {:>5.1}% rss {:>7.1}MiB active turns {}",
                    tok_per_s as u64, events_per_s as u64, cpu, rss_mib, active_turns
                );
            }
            if stop.is_cancelled() {
                return samples;
            }
        }
    })
}

/// Delay between hold turns once a session reached its context watermark.
/// 2s keeps the transcript-clone cost per session at ~0.9 MB/s instead of
/// hundreds of MB/s of pure memcpy.
async fn fill_hold_delay() {
    tokio::time::sleep(Duration::from_secs(2)).await;
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64) * fraction).clamp(0.0, (sorted.len() - 1) as f64) as usize;
    sorted[index]
}

fn gib(bytes: f64) -> String {
    if bytes >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2}G", bytes / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1}M", bytes / (1024.0 * 1024.0))
    }
}

fn format_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn print_report(
    args: &Args,
    wall: Duration,
    samples: &[WindowSample],
    metrics: &Metrics,
    freed: u64,
    watermarks: &[u64],
) {
    let mut tok: Vec<f64> = samples.iter().map(|s| s.tok_per_s).collect();
    tok.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut events: Vec<f64> = samples.iter().map(|s| s.events_per_s).collect();
    events.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let total_tokens = metrics.completion_tokens.load(Ordering::Relaxed);
    let total_events = metrics.events.load(Ordering::Relaxed);
    let avg_tok = total_tokens as f64 / wall.as_secs_f64().max(0.001);
    let avg_events = total_events as f64 / wall.as_secs_f64().max(0.001);
    let target = args.rate as f64 * args.n as f64;
    let cpus: Vec<f32> = samples.iter().map(|s| s.cpu_percent).collect();
    let rsss: Vec<u64> = samples.iter().map(|s| s.rss_bytes).collect();
    let cpu_avg = if cpus.is_empty() {
        0.0
    } else {
        cpus.iter().sum::<f32>() / cpus.len() as f32
    };
    let cpu_peak = cpus.iter().copied().fold(0.0f32, f32::max);
    let rss_avg = if rsss.is_empty() {
        0.0
    } else {
        rsss.iter().sum::<u64>() as f64 / rsss.len() as f64
    };
    let rss_peak = rsss.iter().copied().fold(0u64, u64::max);
    let active_avg = if samples.is_empty() {
        0.0
    } else {
        samples.iter().map(|s| s.active_turns as f64).sum::<f64>() / samples.len() as f64
    };

    println!();
    println!("================ effibuddy-stress report ================");
    println!(
        "mode={:?} n={} rate={} tok/s/stream target={:.0} tok/s",
        args.mode, args.n, args.rate, target
    );
    println!(
        "wall {:.1}s   turns started {} completed {} cancelled {} failed {}",
        wall.as_secs_f64(),
        format_u64(metrics.turns_started.load(Ordering::Relaxed)),
        format_u64(metrics.turns_completed.load(Ordering::Relaxed)),
        format_u64(metrics.turns_cancelled.load(Ordering::Relaxed)),
        format_u64(metrics.turns_failed.load(Ordering::Relaxed)),
    );
    println!(
        "tokens {}   avg {:.0} tok/s (ratio {:.1}%)  p50 {:.0}  p95 {:.0}",
        format_u64(total_tokens),
        avg_tok,
        100.0 * avg_tok / target.max(1.0),
        percentile(&tok, 0.50),
        percentile(&tok, 0.95),
    );
    println!(
        "events {}  avg {:.0}/s  p50 {:.0}  p95 {:.0}   tool_calls {}",
        format_u64(total_events),
        avg_events,
        percentile(&events, 0.50),
        percentile(&events, 0.95),
        format_u64(metrics.tool_calls.load(Ordering::Relaxed)),
    );
    println!(
        "cpu avg {:.1}% peak {:.1}%   rss avg {} peak {}   active turns avg {:.0}   saves {} (ram)   ram freed {}",
        cpu_avg,
        cpu_peak,
        gib(rss_avg),
        gib(rss_peak as f64),
        active_avg,
        format_u64(metrics.saves.load(Ordering::Relaxed)),
        gib(freed as f64),
    );
    if args.fill.is_some() && !watermarks.is_empty() {
        let mut fills: Vec<f64> = watermarks
            .iter()
            .map(|tokens| *tokens as f64 / args.context_window as f64 * 100.0)
            .collect();
        fills.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        println!(
            "context fill ({}/{} sessions): p5 {:.1}% p50 {:.1}% p95 {:.1}% max {:.1}%   window {} tok",
            fills.len(),
            args.n,
            percentile(&fills, 0.05),
            percentile(&fills, 0.50),
            percentile(&fills, 0.95),
            fills.last().copied().unwrap_or(0.0),
            format_u64(args.context_window as u64),
        );
    }
    println!("=========================================================");
}

// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let args = parse_args().map_err(|error| {
        eprintln!("{error}");
        print_help();
        anyhow::anyhow!("bad arguments")
    })?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers.max(1))
        .thread_name("effibuddy-stress")
        .enable_all()
        .build()?;

    runtime.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let started = Instant::now();
    println!(
        "effibuddy-stress: mode={:?} n={} rate={} tok/s/stream chunk={} seed={} workers={} tools={} persistence={}",
        args.mode,
        args.n,
        args.rate,
        args.chunk_tokens,
        args.seed,
        args.workers,
        if args.real_tools {
            "real (ram root)"
        } else {
            "virtual (memory)"
        },
        if args.mode == Mode::Subagents {
            "ram store"
        } else {
            "off"
        },
    );

    // RAM workspace (subagents mode persists child-style sessions there;
    // real-tools mode roots file operations there).
    let ram = if args.mode == Mode::Subagents || args.real_tools {
        Some(Arc::new(RamDir::acquire("headless")?))
    } else {
        None
    };
    if let Some(ram) = &ram {
        println!("ram workspace: {}", ram.path().display());
    }

    let provider = Arc::new({
        let mut provider = StressProvider::new()
            .with_rate(args.rate)
            .with_chunk_tokens(args.chunk_tokens)
            .with_seed(args.seed ^ 0x1234_5678)
            .with_max_steps(args.max_steps)
            .with_context_window(args.context_window);
        if let Some((min, max)) = args.fill {
            provider = provider.with_context_fill(min, max);
        }
        provider
    });
    let metrics = Arc::new(Metrics::default());
    let stop = CancellationToken::new();
    let seconds = args.seconds;

    // Deadline + Ctrl-C both cancel `stop`.
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            let deadline = Duration::from_secs_f64(seconds.max(0.1));
            tokio::select! {
                _ = tokio::time::sleep(deadline) => {}
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("[stress] ctrl-c received, finishing…");
                }
            }
            stop.cancel();
        });
    }

    let sampler = spawn_sampler(metrics.clone(), stop.clone());

    let watermarks: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::with_capacity(args.n);
    let mut handles: Vec<AgentHandle> = Vec::with_capacity(args.n);
    for index in 0..args.n {
        match start_session(
            index,
            &args,
            provider.clone(),
            stop.clone(),
            metrics.clone(),
            ram.clone(),
            watermarks.clone(),
        ) {
            Ok((task, handle)) => {
                tasks.push(task);
                handles.push(handle);
            }
            Err(error) => eprintln!("[stress] session {index}: setup failed: {error}"),
        }
    }

    // Deadline watcher: interrupt every in-flight turn promptly so the wall
    // time stays honest.
    {
        let stop = stop.clone();
        let handles = Arc::new(Mutex::new(std::mem::take(&mut handles)));
        tokio::spawn(async move {
            stop.cancelled().await;
            let mut guard = handles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for handle in guard.drain(..) {
                handle.interrupt();
            }
        });
    }

    for task in tasks {
        let _ = task.await;
    }
    stop.cancel();
    let samples = sampler.await.unwrap_or_default();
    let wall = started.elapsed();

    let freed = ram.as_ref().map(|ram| ram.release()).unwrap_or(0);
    let watermarks = watermarks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    print_report(&args, wall, &samples, &metrics, freed, &watermarks);
    Ok(())
}
