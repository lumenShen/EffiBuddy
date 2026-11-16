//! Frame instrumentation and the performance lab (release-safe).
//!
//! Everything here is env-gated, so release builds carry it at zero cost
//! until asked:
//!
//! * `EFFIBUDDY_PERF=1` — collect per-frame stats, print a rolling two-second
//!   summary on stderr (element-tree build cost + markdown cache hits).
//! * `EFFIBUDDY_PERF_HUD=1` — draw a live overlay (fps, frame cost, cache
//!   hits) instead of / in addition to stderr.
//! * `--bench [seconds]` — run the stress transcript with a continuously
//!   refilled typewriter backlog, print one `[bench]` summary line and exit:
//!   the invokable benchmark entry point.
//! * `EFFIBUDDY_CPU_SLOW_MS=<ms>` — emulate a low-performance CPU: busy-spin
//!   that many milliseconds at the start of every frame, so each frame's work
//!   takes that much longer (occupies the thread exactly like real work).
//! * `EFFIBUDDY_GPU_SLOW_MS=<ms>` — emulate a low-performance GPU; patched
//!   into the vendored gpui present/fence wait (see `vendor/gpui`), so the
//!   main thread parks longer per frame with the CPU free — the real
//!   signature of a GPU-bound pipeline.
//! * `EFFIBUDDY_VSYNC=0` — present frames without waiting for the display,
//!   restoring the unlocked loop (several hundred fps during animations).
//!   Vertical sync is on by default so animations are paced to the monitor;
//!   the unlocked mode exists so this lab can still measure raw throughput,
//!   and runs that use it are tagged `NO-VSYNC` in the HUD and bench logs.

use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use effibuddy_ui::markdown_cache_hits;

/// How often the stderr summary line is printed.
const WINDOW: Duration = Duration::from_secs(2);
/// Interval samples above this are idle gaps, not frame pacing; drop them so
/// percentiles describe saturated runs.
const MAX_TRACKED_INTERVAL_MS: f64 = 1_000.0;
/// Cap on stored samples (10s at 120fps is ~1.2k; this allows long runs).
const MAX_SAMPLES: usize = 400_000;

fn env_u64(name: &str) -> u64 {
    static CACHE: OnceLock<[u64; 8]> = OnceLock::new();
    // Small fixed table of the env knobs we read (indexed by name hash) to
    // keep the hot path allocation-free. Only two knobs exist; parse lazily.
    let _ = CACHE.get_or_init(|| {
        [
            env_parsed("EFFIBUDDY_CPU_SLOW_MS"),
            env_parsed("EFFIBUDDY_GPU_SLOW_MS"),
            0,
            0,
            0,
            0,
            0,
            0,
        ]
    });
    match name {
        "EFFIBUDDY_CPU_SLOW_MS" => CACHE.get().unwrap()[0],
        "EFFIBUDDY_GPU_SLOW_MS" => CACHE.get().unwrap()[1],
        _ => 0,
    }
}

fn env_parsed(name: &str) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Emulate a low-performance CPU: busy-spin `EFFIBUDDY_CPU_SLOW_MS` at the
/// start of every frame. Spinning (not sleeping) occupies the main thread
/// exactly like genuinely slower per-frame work would.
fn cpu_slow_spin() {
    let extra_ms = env_u64("EFFIBUDDY_CPU_SLOW_MS");
    if extra_ms == 0 {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(extra_ms);
    let mut spins = 0u64;
    while Instant::now() < deadline {
        std::hint::spin_loop();
        spins += 1;
        // Yield every ~200µs so the process stays a good citizen on a
        // loaded machine without meaningfully changing the emulation.
        if spins % 64_000 == 0 {
            std::thread::yield_now();
        }
    }
}

#[derive(Default)]
struct PerfState {
    /// Rolling stderr-summary window.
    window_start: Option<Instant>,
    window_samples: Vec<f64>,
    window_hits: u64,
    /// Last-second frame intervals + cache hits, for the HUD.
    recent_intervals: Vec<(Instant, f64)>,
    recent_hits_stamp: Option<Instant>,
    recent_hits_base: u64,
    recent_hits_value: u64,
    /// Bench-wide accumulators.
    frames: u64,
    intervals_ms: Vec<f64>,
    render_ms: Vec<f64>,
    /// Full-frame work (render + layout + shaping + present submission) from
    /// the gpui frame cycle, in milliseconds.
    work_ms: Vec<f64>,
    first_frame: Option<Instant>,
    last_frame: Option<Instant>,
    saturated: bool,
}

impl PerfState {
    fn note_frame_start(&mut self, now: Instant) {
        if let Some(last) = self.last_frame {
            let interval = (now - last).as_secs_f64() * 1000.0;
            if interval <= MAX_TRACKED_INTERVAL_MS {
                if self.intervals_ms.len() < MAX_SAMPLES {
                    self.intervals_ms.push(interval);
                }
                self.recent_intervals.push((now, interval));
            }
        }
        self.recent_intervals
            .retain(|(stamp, _)| now.duration_since(*stamp) <= Duration::from_secs(1));
        self.last_frame = Some(now);
        self.first_frame.get_or_insert(now);
        self.frames += 1;
        self.saturated |= self.intervals_ms.len() >= MAX_SAMPLES;
    }

    fn hits_per_second(&mut self, now: Instant) -> u64 {
        let hits = markdown_cache_hits();
        match self.recent_hits_stamp {
            Some(stamp) if now.duration_since(stamp) < Duration::from_secs(1) => {
                self.recent_hits_value
            }
            _ => {
                let seconds = self
                    .recent_hits_stamp
                    .map(|stamp| (now - stamp).as_secs_f64().max(0.25))
                    .unwrap_or(1.0);
                let per_second =
                    ((hits.saturating_sub(self.recent_hits_base)) as f64 / seconds) as u64;
                self.recent_hits_stamp = Some(now);
                self.recent_hits_base = hits;
                self.recent_hits_value = per_second;
                per_second
            }
        }
    }
}

static STATE: LazyLock<Mutex<PerfState>> = LazyLock::new(|| Mutex::new(PerfState::default()));

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("EFFIBUDDY_PERF").is_ok_and(|value| value == "1"))
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    // Empty samples are normal (a run that never rendered, or profiling off):
    // return 0 rather than underflowing `len - 1`.
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

/// Returns the wall-clock start stamp when profiling is on, else `None`.
/// Also applies the CPU-slow emulation so it lands inside every measured
/// frame exactly like slower per-frame work.
pub fn frame_start() -> Option<Instant> {
    let started = enabled().then(Instant::now)?;
    cpu_slow_spin();
    let now = Instant::now();
    if let Ok(mut state) = STATE.lock() {
        state.note_frame_start(now);
    }
    Some(started)
}

/// Records one finished frame; prints a summary every [`WINDOW`].
pub fn frame_end(started: Instant) {
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    if state.render_ms.len() < MAX_SAMPLES {
        state.render_ms.push(elapsed);
    }
    let work =
        f64::from(gpui::LAST_FRAME_WORK_MICROS.load(std::sync::atomic::Ordering::Relaxed)) / 1000.0;
    if state.work_ms.len() < MAX_SAMPLES {
        state.work_ms.push(work);
    }
    let window_ended = state
        .window_start
        .is_none_or(|start| start.elapsed() >= WINDOW);
    if window_ended {
        let count = state.window_samples.len();
        if count > 0 {
            let window_seconds = state
                .window_start
                .map(|start| start.elapsed().as_secs_f64())
                .unwrap_or(WINDOW.as_secs_f64());
            let mut sorted = state.window_samples.clone();
            sorted.sort_by(f64::total_cmp);
            let percentile = |fraction: f64| -> f64 { percentile(&sorted, fraction) };
            let average = state.window_samples.iter().sum::<f64>() / count as f64;
            let hits = markdown_cache_hits().saturating_sub(state.window_hits);
            eprintln!(
                "[perf] {count} frames in {:.1}s: render avg {:.2}ms p50 {:.2}ms p95 {:.2}ms markdown-cache-hits {hits}",
                window_seconds,
                average,
                percentile(0.50),
                percentile(0.95),
            );
        }
        state.window_start = Some(Instant::now());
        state.window_samples.clear();
        state.window_hits = markdown_cache_hits();
    }
    state.window_samples.push(elapsed);
}

/// Live overlay numbers; `None` while profiling is off.
pub struct HudSnapshot {
    pub fps: f64,
    pub interval_p50_ms: f64,
    pub interval_p95_ms: f64,
    pub render_avg_ms: f64,
    pub render_p95_ms: f64,
    pub cache_hits_per_s: u64,
}

pub fn hud_snapshot() -> Option<HudSnapshot> {
    if !enabled() {
        return None;
    }
    let now = Instant::now();
    let Ok(mut state) = STATE.lock() else {
        return None;
    };
    state
        .recent_intervals
        .retain(|(stamp, _)| now.duration_since(*stamp) <= Duration::from_secs(1));
    let intervals: Vec<f64> = state
        .recent_intervals
        .iter()
        .map(|(_, interval)| *interval)
        .collect();
    if intervals.is_empty() {
        return Some(HudSnapshot {
            fps: 0.0,
            interval_p50_ms: 0.0,
            interval_p95_ms: 0.0,
            render_avg_ms: 0.0,
            render_p95_ms: 0.0,
            cache_hits_per_s: state.hits_per_second(now),
        });
    }
    let mut sorted = intervals.clone();
    sorted.sort_by(f64::total_cmp);
    let mut render = state.window_samples.clone();
    render.sort_by(f64::total_cmp);
    let seconds = (intervals.iter().sum::<f64>() / 1000.0).max(0.001);
    let fps = intervals.len() as f64 / seconds;
    Some(HudSnapshot {
        fps,
        interval_p50_ms: percentile(&sorted, 0.50),
        interval_p95_ms: percentile(&sorted, 0.95),
        render_avg_ms: if render.is_empty() {
            0.0
        } else {
            render.iter().sum::<f64>() / render.len() as f64
        },
        render_p95_ms: if render.is_empty() {
            0.0
        } else {
            percentile(&render, 0.95)
        },
        cache_hits_per_s: state.hits_per_second(now),
    })
}

/// One-line benchmark summary for `--bench`; the invokable perf entry point.
pub fn bench_summary() -> String {
    // `frame_start`/`frame_end` are no-ops unless `EFFIBUDDY_PERF=1`, so a
    // bench run started without it has no samples at all. Say so instead of
    // indexing an empty percentile table (which used to panic the app right
    // when the run finished).
    if !enabled() {
        return "[bench] no samples: rerun with EFFIBUDDY_PERF=1".to_string();
    }
    let Ok(state) = STATE.lock() else {
        return "[bench] stats unavailable".to_string();
    };
    let elapsed = state
        .first_frame
        .zip(state.last_frame)
        .map(|(first, last)| (last - first).as_secs_f64())
        .unwrap_or(0.0);
    let fps = if elapsed > 0.05 {
        state.frames as f64 / elapsed
    } else {
        0.0
    };
    let mut intervals = state.intervals_ms.clone();
    intervals.sort_by(f64::total_cmp);
    let mut renders = state.render_ms.clone();
    renders.sort_by(f64::total_cmp);
    let mut works = state.work_ms.clone();
    works.sort_by(f64::total_cmp);
    let iv = |fraction: f64| percentile(&intervals, fraction);
    let rd = |fraction: f64| percentile(&renders, fraction);
    let wk = |fraction: f64| percentile(&works, fraction);
    let avg = |samples: &[f64]| {
        if samples.is_empty() {
            0.0
        } else {
            samples.iter().sum::<f64>() / samples.len() as f64
        }
    };
    format!(
        "[bench] frames {} in {:.2}s → fps {:.1} | frame ms avg {:.2} p50 {:.2} p95 {:.2} p99 {:.2} | work ms avg {:.2} p50 {:.2} p95 {:.2} p99 {:.2} | render ms avg {:.2} p50 {:.2} p95 {:.2} | markdown-cache-hits {}{}",
        state.frames,
        elapsed,
        fps,
        avg(&state.intervals_ms),
        iv(0.50),
        iv(0.95),
        iv(0.99),
        avg(&state.work_ms),
        wk(0.50),
        wk(0.95),
        wk(0.99),
        avg(&state.render_ms),
        rd(0.50),
        rd(0.95),
        markdown_cache_hits(),
        if state.saturated {
            " (sample cap hit)"
        } else {
            ""
        },
    )
}

/// Whether the app is currently presenting in sync with the display.
///
/// Read from the platform layer so the HUD, the bench summary and the settings
/// page all agree: `EFFIBUDDY_VSYNC=0` forces presentation off-sync for the
/// performance lab, and the user's setting can turn it off at runtime.
pub fn vsync_enabled() -> bool {
    gpui::platform_vsync_enabled()
}

/// Active degradation-mode badges, for the HUD and bench logs.
pub fn mode_badges() -> String {
    let mut badges = Vec::new();
    let cpu = env_u64("EFFIBUDDY_CPU_SLOW_MS");
    if cpu > 0 {
        badges.push(format!("CPU+{cpu}ms"));
    }
    let gpu = env_u64("EFFIBUDDY_GPU_SLOW_MS");
    if gpu > 0 {
        badges.push(format!("GPU+{gpu}ms"));
    }
    if std::env::var_os("EFFIBUDDY_NO_SPLIT").is_some() {
        badges.push("NOSPLIT".to_string());
    }
    if !vsync_enabled() {
        // Unlocked presentation: frame counts here are not comparable with a
        // vsynced run on the same machine.
        badges.push("NO-VSYNC".to_string());
    }
    if badges.is_empty() {
        "baseline".to_string()
    } else {
        badges.join(" ")
    }
}
