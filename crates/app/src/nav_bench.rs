//! Real-window navigation memory lab (`--bench-pages`).
//!
//! Drives the real window through the user's actual navigation paths —
//! switching conversations in the sidebar, opening every settings section and
//! toggling the graph — and prints one line per step: RSS, glibc heap, the
//! transcripts still resident and the UI caches. `--bench-scroll` answers "is
//! scrolling fast enough"; this answers "what does browsing cost", which is
//! the shape a leak takes here (transcripts, decoded screenshots, arenas).
//!
//! Run it against a copy of a real data directory:
//!
//! ```text
//! EFFIBUDDY_DATA_DIR=/tmp/copy effibuddy --bench-pages
//! ```
//!
//! Keep the window visible and pass `EFFIBUDDY_VSYNC=0`: an occluded window
//! stops receiving frame callbacks, and the frame column then freezes (the
//! numbers after that point describe an unpainted window).

use std::time::{Duration, Instant};

use effibuddy_agent_core::lock_session;
use gpui::{Context, Window};

use crate::app::AppView;
use crate::graph::ViewMode;
use crate::memory::{app_memory, sample_heap, sample_process};
use crate::settings::SettingsSection;

/// Frames that reached `AppView::render`, to tell a frozen window (the
/// compositor stops sending frame callbacks when it is occluded) apart from a
/// stalled chat.
pub(crate) static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set only for a `--bench-pages` run, so an ordinary session pays one relaxed
/// load per frame and nothing else.
static BENCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn note_frame() {
    if BENCHING.load(std::sync::atomic::Ordering::Relaxed) {
        FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
pub(crate) enum NavStep {
    Session(String),
    Settings(SettingsSection),
    SettingsClose,
    Graph(ViewMode),
    Settle,
}

impl NavStep {
    fn label(&self) -> String {
        match self {
            Self::Session(id) => format!("会话 {}", &id[..id.len().min(8)]),
            Self::Settings(section) => format!("设置 {section:?}"),
            Self::SettingsClose => "关闭设置".to_string(),
            Self::Graph(ViewMode::Graph) => "打开图谱".to_string(),
            Self::Graph(ViewMode::Chat) => "关闭图谱".to_string(),
            Self::Settle => "空闲".to_string(),
        }
    }
}

impl AppView {
    pub(crate) fn start_nav_bench(&mut self, cx: &mut Context<Self>) {
        BENCHING.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut steps = Vec::new();
        for summary in &self.sessions {
            steps.push(NavStep::Session(summary.id.clone()));
        }
        steps.push(NavStep::Settle);
        for section in [
            SettingsSection::General,
            SettingsSection::Models,
            SettingsSection::Context,
            SettingsSection::Agents,
            SettingsSection::Archived,
            SettingsSection::Appearance,
            SettingsSection::Memory,
            SettingsSection::About,
        ] {
            steps.push(NavStep::Settings(section));
        }
        steps.push(NavStep::SettingsClose);
        steps.push(NavStep::Graph(ViewMode::Graph));
        steps.push(NavStep::Settle);
        steps.push(NavStep::Graph(ViewMode::Chat));
        // Revisit a few conversations: a detached transcript must reload from
        // disk and must not push RSS past the first pass.
        for summary in self.sessions.iter().take(6) {
            steps.push(NavStep::Session(summary.id.clone()));
        }
        steps.push(NavStep::Settle);

        cx.spawn(async move |weak, cx| {
            if let Ok(line) = weak.update(cx, |this, _| report("基线", this)) {
                eprintln!("{line}");
            }
            for step in steps {
                // Let frames land so deferred (async) work is included.
                cx.background_executor()
                    .timer(Duration::from_millis(160))
                    .await;
                let handle = match weak.update(cx, |_, cx| {
                    cx.windows()
                        .into_iter()
                        .find(|handle| handle.downcast::<AppView>().is_some())
                }) {
                    Ok(Some(handle)) => handle,
                    _ => break,
                };
                let step_for_update = step.clone();
                let applied = handle
                    .update(cx, |root, window, cx| {
                        let Ok(view) = root.downcast::<AppView>() else {
                            return;
                        };
                        let _ = view.update(cx, |this, cx| {
                            this.nav_bench_apply(&step_for_update, window, cx);
                        });
                    })
                    .is_ok();
                if !applied {
                    break;
                }
                // Paint a fixed number of frames per step: the compositor
                // throttles the window at unpredictable moments, and RSS
                // follows how much was actually painted, so un-paced runs are
                // not comparable.
                let before = FRAMES.load(std::sync::atomic::Ordering::Relaxed);
                let deadline = Instant::now() + Duration::from_millis(700);
                while FRAMES.load(std::sync::atomic::Ordering::Relaxed) - before < 10
                    && Instant::now() < deadline
                {
                    cx.background_executor()
                        .timer(Duration::from_millis(5))
                        .await;
                }
                let label = step.label();
                if let Ok(line) = weak.update(cx, |this, _| report(&label, this)) {
                    eprintln!("{line}");
                }
            }
            if let Ok(line) = weak.update(cx, |this, _| report("最终", this)) {
                eprintln!("{line}");
            }
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
        cx.notify();
    }

    fn nav_bench_apply(
        &mut self,
        step: &NavStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match step {
            NavStep::Session(id) => self.select_session(id, window, cx),
            NavStep::Settings(section) => self.open_settings_section(*section, window, cx),
            NavStep::SettingsClose => {
                self.settings_open = false;
                cx.notify();
            }
            NavStep::Graph(mode) => self.set_view_mode(*mode, cx),
            NavStep::Settle => cx.notify(),
        }
    }
}

fn report(label: &str, view: &AppView) -> String {
    let memory = app_memory(view);
    let process = sample_process();
    let heap = sample_heap();
    let rss = process.map_or(0, |process| process.rss_bytes);
    let heap_used = heap.map_or(0, |heap| heap.in_use_bytes);
    let heap_free = heap.map_or(0, |heap| heap.free_bytes);
    let mmap = heap.map_or(0, |heap| heap.mmap_bytes);
    let shares = view.session_shares.len();
    let messages = view
        .session_shares
        .values()
        .map(|shared| lock_session(shared).messages.len())
        .sum::<usize>();
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    format!(
        "[nav] {label:<20} 帧 {:>4} 线程 {:>2} RSS {:>7.1}MB 堆 {:>6.1}+{:>6.1} mmap {:>6.1}MB | 份额 {:>2} 份 {:>5} 条 | 载入 {:>2} 份 {:>6.1}MB {:>4} 条 | 列表 {:>5} 项 {:>5.1}MB | 图 {:>2} 张 {:>5.1}MB | md {:>3} 篇 {:>5.1}MB",
        FRAMES.load(std::sync::atomic::Ordering::Relaxed),
        process.map_or(0, |process| process.threads),
        mib(rss),
        mib(heap_used),
        mib(heap_free),
        mib(mmap),
        shares,
        messages,
        memory.loaded_transcripts,
        mib(memory.transcript_bytes),
        memory.transcript_messages,
        memory.chat_items,
        mib(memory.chat_bytes),
        memory.decoded_images,
        mib(memory.decoded_image_bytes),
        memory.markdown_documents,
        mib(memory.markdown_document_bytes),
    )
}
