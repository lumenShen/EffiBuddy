//! Memory diagnostics: process RSS, allocator state and the app's own caches.
//!
//! A GPUI desktop app accumulates memory in three very different places:
//!
//! 1. live application state (transcripts, the virtual chat snapshot, markdown
//!    IR caches, decoded images),
//! 2. glibc's per-thread arenas, which keep freed chunks instead of returning
//!    them to the OS,
//! 3. the GPU driver / atlas.
//!
//! A single RSS number cannot tell those apart, which makes "EffiBuddy uses
//! 1.3 GB" impossible to act on. This module samples each layer for the
//! Settings → 内存 panel and caps the allocator's arena count at startup: with
//! one arena per worker thread, a burst of large temporary buffers left ~1 GB
//! of free-but-unreturnable chunks behind.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use effibuddy_agent_core::{ContentBlock, Message, SessionState, lock_session};
use effibuddy_ui::{Button, ButtonSize, ButtonVariant, Theme};
use gpui::{AnyElement, ClipboardItem, Context, Hsla, IntoElement, div, prelude::*, px, relative};

use crate::app::AppView;
use crate::attachments::format_bytes;

/// Sampling interval floor for the background trim after a run.
const TRIM_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessMemory {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub vm_data_bytes: u64,
    pub vm_size_bytes: u64,
    pub threads: u32,
}

/// Aggregated glibc allocator state. All values are process-wide and come
/// from `mallinfo2`, so they include every arena and mmap'd block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeapMemory {
    /// Non-mmapped bytes obtained from the system (all arenas).
    pub arena_bytes: u64,
    /// Bytes inside live allocations.
    pub in_use_bytes: u64,
    /// Free chunks still held by arenas. This is the part a trim can return.
    pub free_bytes: u64,
    /// Bytes in mmap'd blocks (the allocator's large-allocation path).
    pub mmap_bytes: u64,
}

/// Cap the glibc arena count before any worker thread starts allocating.
///
/// glibc defaults to `8 × cores` arenas (256 here). Every GPUI worker gets its
/// own 64 MB arena; a large temporary buffer allocated and freed on each
/// worker leaves that arena holding the freed chunk, and with tens of workers
/// the retained total reached ~1 GB while live data was ~200 MB. Four arenas
/// keep the parallelism the worker pool needs while letting freed chunks be
/// reused across threads. Must be called before the runtime spawns threads.
pub(crate) fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only adjusts allocator tuning; called before the
        // application spawns its worker threads.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 4);
        }
    }
}

pub(crate) fn sample_process() -> Option<ProcessMemory> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_status(&raw)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn parse_status(raw: &str) -> Option<ProcessMemory> {
    let field = |key: &str| -> Option<u64> {
        raw.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case(key).then(|| {
                value
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|number| number.parse::<u64>().ok())
            })?
        })
    };
    Some(ProcessMemory {
        rss_bytes: field("VmRSS")?.saturating_mul(1024),
        peak_rss_bytes: field("VmHWM").unwrap_or(0).saturating_mul(1024),
        vm_data_bytes: field("VmData").unwrap_or(0).saturating_mul(1024),
        vm_size_bytes: field("VmSize").unwrap_or(0).saturating_mul(1024),
        threads: field("Threads").unwrap_or(0) as u32,
    })
}

pub(crate) fn sample_heap() -> Option<HeapMemory> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo2 reads allocator bookkeeping and has no inputs.
        let info = unsafe { libc::mallinfo2() };
        Some(HeapMemory {
            arena_bytes: info.arena as u64,
            in_use_bytes: info.uordblks as u64,
            free_bytes: info.fordblks as u64,
            mmap_bytes: info.hblkhd as u64,
        })
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// Ask the allocator to return free pages at the top of every arena.
///
/// Returns whether anything was released. Chunks trapped between live objects
/// cannot be returned, which is why the arena cap above is the real fix; this
/// is the manual "give memory back now" button and the post-run cleanup.
pub(crate) fn trim_heap() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim only releases free allocator pages.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

static LAST_TRIM: Mutex<Option<Instant>> = Mutex::new(None);

/// Trim on a background thread, at most once every [`TRIM_INTERVAL`].
///
/// Called when a run finishes: the end of a turn is exactly when the large
/// JSON/HTML/markdown temporaries have been dropped, so it is the cheapest
/// moment to hand their pages back to the OS. The throttle keeps short
/// queued turns from paying the walk repeatedly.
pub(crate) fn trim_heap_async() {
    let now = Instant::now();
    {
        let Ok(mut last) = LAST_TRIM.lock() else {
            return;
        };
        if let Some(previous) = *last {
            if now.duration_since(previous) < TRIM_INTERVAL {
                return;
            }
        }
        *last = Some(now);
    }
    std::thread::spawn(|| {
        let _ = trim_heap();
    });
}

/// Rough memory held by one transcript: text payload, tool output, tool-call
/// arguments and base64 image parts. Per-message/block overhead is a constant
/// estimate, which is enough to compare components in the panel.
pub(crate) fn message_bytes(message: &Message) -> u64 {
    let mut bytes = 64 + message.id.len() as u64;
    for block in &message.blocks {
        bytes += match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len() as u64,
            ContentBlock::ToolCall { call } => {
                call.id.len() as u64 + call.name.len() as u64 + json_bytes(&call.arguments)
            }
            ContentBlock::ToolResult { result } => {
                result.content.len() as u64
                    + result
                        .images
                        .iter()
                        .map(|image| image.data.len() as u64)
                        .sum::<u64>()
                    + result.metadata.as_ref().map(json_bytes).unwrap_or(0)
            }
        } + 24;
    }
    for attachment in &message.attachments {
        bytes += attachment.text.as_ref().map_or(0, String::len) as u64
            + attachment
                .image
                .as_ref()
                .map_or(0, |image| image.data.len()) as u64;
    }
    bytes
}

/// Approximate in-memory size of a JSON value without allocating a string.
fn json_bytes(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 8,
        serde_json::Value::Number(_) => 16,
        serde_json::Value::String(text) => text.len() as u64,
        serde_json::Value::Array(items) => 16 + items.iter().map(json_bytes).sum::<u64>(),
        serde_json::Value::Object(map) => {
            32 + map
                .iter()
                .map(|(key, value)| key.len() as u64 + json_bytes(value))
                .sum::<u64>()
        }
    }
}

/// Live memory the app can attribute to its own data structures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AppMemory {
    /// Conversation summaries known to the sidebar + archive.
    pub sessions: usize,
    /// Distinct transcripts currently loaded in memory (active, running in the
    /// background, or held by child agents).
    pub loaded_transcripts: usize,
    pub transcript_messages: usize,
    pub transcript_bytes: u64,
    pub chat_items: usize,
    pub chat_bytes: u64,
    pub chat_tool_results: usize,
    pub chat_tool_result_bytes: u64,
    pub markdown_documents: usize,
    pub markdown_document_bytes: u64,
    pub markdown_chunks: usize,
    pub markdown_chunk_bytes: u64,
    pub decoded_images: usize,
    pub decoded_image_bytes: u64,
    pub subagents: usize,
    pub subagent_bytes: u64,
    pub background_runs: usize,
}

/// Snapshot every component the host can attribute. Caches owned by the UI
/// crate (markdown IR) are read through their public counters.
pub(crate) fn app_memory(view: &AppView) -> AppMemory {
    let mut memory = AppMemory {
        sessions: view.sessions.len() + view.archived_sessions.len(),
        background_runs: view.background_runs.len(),
        ..AppMemory::default()
    };

    // Every live transcript is a `SharedSession`; the same Arc may appear in
    // several maps (active + parked UI + child task), so dedupe by identity.
    let mut seen: Vec<*const std::sync::Mutex<SessionState>> = Vec::new();
    let add_session =
        |shared: &effibuddy_agent_core::SharedSession,
         memory: &mut AppMemory,
         seen: &mut Vec<*const std::sync::Mutex<SessionState>>| {
            let pointer = std::sync::Arc::as_ptr(shared);
            if seen.contains(&pointer) {
                return;
            }
            seen.push(pointer);
            let session = lock_session(shared);
            memory.loaded_transcripts += 1;
            memory.transcript_messages += session.messages.len();
            memory.transcript_bytes += session.messages.iter().map(message_bytes).sum::<u64>();
        };

    if let Some(active) = &view.active {
        add_session(&active.shared, &mut memory, &mut seen);
    }
    for shared in view.session_shares.values() {
        add_session(shared, &mut memory, &mut seen);
    }
    for tasks in std::iter::once(&view.subagent_tasks).chain(
        view.background_sessions
            .values()
            .map(|state| &state.subagent_tasks),
    ) {
        for task in tasks {
            memory.subagents += 1;
            add_session(&task.shared, &mut memory, &mut seen);
        }
    }

    let chat = view.chat.memory_stats();
    memory.chat_items = chat.items;
    memory.chat_bytes = chat.text_bytes;
    memory.chat_tool_results = chat.tool_results;
    memory.chat_tool_result_bytes = chat.tool_result_bytes;
    memory.decoded_images = chat.image_cache_entries;
    memory.decoded_image_bytes = chat.image_cache_bytes;

    let markdown = effibuddy_ui::markdown_cache_stats();
    memory.markdown_documents = markdown.documents;
    memory.markdown_document_bytes = markdown.document_bytes;
    memory.markdown_chunks = markdown.chunks;
    memory.markdown_chunk_bytes = markdown.chunk_bytes;

    // Child transcripts are counted in `transcript_bytes` too; report the
    // child-only part as its own line so a runaway subagent is visible.
    for tasks in std::iter::once(&view.subagent_tasks).chain(
        view.background_sessions
            .values()
            .map(|state| &state.subagent_tasks),
    ) {
        for task in tasks {
            let session = lock_session(&task.shared);
            memory.subagent_bytes += session.messages.iter().map(message_bytes).sum::<u64>();
        }
    }

    memory
}

// --- Settings → 内存 panel -------------------------------------------------

/// Transient panel state lives here instead of on `AppView`: the section is a
/// thin settings hook and nothing else needs these values.
static PANEL_TICKER: AtomicBool = AtomicBool::new(false);
static TRIMMING: AtomicBool = AtomicBool::new(false);
static TRIM_STATUS: Mutex<Option<(bool, String)>> = Mutex::new(None);
static PANEL_SNAPSHOT: Mutex<Option<(Instant, AppMemory)>> = Mutex::new(None);
static LAST_RENDER: Mutex<Option<Instant>> = Mutex::new(None);

/// Render the memory diagnostics page.
///
/// A free function (rather than a method on `AppView`) keeps the whole page in
/// one module and the settings section down to a single `match` arm.
pub(crate) fn render_panel(
    view: &mut AppView,
    theme: &Theme,
    cx: &mut Context<AppView>,
) -> AnyElement {
    if let Ok(mut last) = LAST_RENDER.lock() {
        *last = Some(Instant::now());
    }
    ensure_ticker(cx);

    let process = sample_process();
    let heap = sample_heap();
    let app = cached_app_memory(view);

    let attributed = app.transcript_bytes
        + app.chat_bytes
        + app.chat_tool_result_bytes
        + app.markdown_document_bytes
        + app.markdown_chunk_bytes
        + app.decoded_image_bytes;
    let heap_free = heap.map_or(0, |heap| heap.free_bytes);
    let heap_in_use = heap.map_or(0, |heap| heap.in_use_bytes);
    let heap_mmap = heap.map_or(0, |heap| heap.mmap_bytes);
    let rss = process.map_or(0, |process| process.rss_bytes);
    let denominator = rss.max(heap_in_use + heap_free + heap_mmap).max(1) as f32;
    let attributed_share = (attributed as f32 / denominator).min(1.0);
    let free_share = (heap_free as f32 / denominator).min(1.0 - attributed_share);

    // --- process ---------------------------------------------------------
    let mut process_card = panel_card(theme, "进程内存", "每秒刷新 · 当前进程");
    match process {
        Some(process) => {
            process_card = process_card
                .child(metrics(
                    theme,
                    &[
                        ("常驻内存 RSS", format_bytes(process.rss_bytes), theme.text),
                        (
                            "峰值 HWM",
                            format_bytes(process.peak_rss_bytes),
                            theme.text_muted,
                        ),
                        (
                            "数据段 VmData",
                            format_bytes(process.vm_data_bytes),
                            theme.text_muted,
                        ),
                        ("线程数", process.threads.to_string(), theme.text_muted),
                    ],
                ))
                .child(memory_bar(
                    theme,
                    &[
                        (attributed_share, theme.accent, "应用内已归因"),
                        (free_share, theme.warning, "分配器空闲未归还"),
                        (
                            (1.0 - attributed_share - free_share).max(0.0),
                            theme.text_faint,
                            "其他（驱动/字体/共享库）",
                        ),
                    ],
                ))
                .child(panel_row(
                    theme,
                    "应用内已归因（文本与图像载荷）",
                    &format!(
                        "{} · 约 {:.0}% RSS",
                        format_bytes(attributed),
                        attributed_share * 100.0
                    ),
                ))
                .child(panel_row(
                    theme,
                    "分配器空闲未归还",
                    &format!(
                        "{} · 约 {:.0}% RSS",
                        format_bytes(heap_free),
                        free_share * 100.0
                    ),
                ))
                .child(
                    div()
                        .text_size(px(10.5))
                        .line_height(px(16.0))
                        .text_color(theme.text_faint)
                        .child(
                            "条形按 RSS 估算。应用内数据是代码估算的文本/图像载荷；其他部分包含\
                             Vulkan 驱动、字体栅格、共享库与未被归因的临时对象。",
                        ),
                );
        }
        None => {
            process_card = process_card.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child("当前平台不支持进程内存采样（仅 Linux 提供 /proc 数据）。"),
            );
        }
    }

    // --- allocator -------------------------------------------------------
    let mut allocator_card = panel_card(theme, "分配器", "glibc mallinfo2 · 所有 arena 合计");
    match heap {
        Some(heap) => {
            let total = (heap.in_use_bytes + heap.free_bytes + heap.mmap_bytes).max(1) as f32;
            let warning = heap.free_bytes >= 256 * 1024 * 1024;
            allocator_card = allocator_card
                .child(metrics(
                    theme,
                    &[
                        (
                            "向系统申请",
                            format_bytes(heap.arena_bytes),
                            theme.text_muted,
                        ),
                        ("正在使用", format_bytes(heap.in_use_bytes), theme.text),
                        (
                            "空闲未归还",
                            format_bytes(heap.free_bytes),
                            if warning {
                                theme.warning
                            } else {
                                theme.text_muted
                            },
                        ),
                        ("mmap 大块", format_bytes(heap.mmap_bytes), theme.text_muted),
                    ],
                ))
                .child(memory_bar(
                    theme,
                    &[
                        (heap.in_use_bytes as f32 / total, theme.accent, "正在使用"),
                        (heap.free_bytes as f32 / total, theme.warning, "空闲未归还"),
                        (
                            heap.mmap_bytes as f32 / total,
                            theme.agent_purple,
                            "mmap 大块",
                        ),
                    ],
                ));
            if warning {
                allocator_card = allocator_card.child(
                    div()
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.warning)
                        .bg(theme.warning_soft)
                        .px(px(12.0))
                        .py(px(9.0))
                        .text_size(px(11.0))
                        .line_height(px(17.0))
                        .text_color(theme.text)
                        .child(
                            "空闲块超过 256 MB：这些是已释放、但被 glibc 各线程 arena 留在进程里\
                             的页，通常来自大块的会话序列化 / Markdown / JSON 临时缓冲。新版启动时\
                             已把 arena 上限压到 4，避免每个工作线程各留一份；当前进程仍可点\
                             「归还空闲内存」尝试释放。",
                        ),
                );
            }
            let trimming = TRIMMING.load(Ordering::SeqCst);
            allocator_card = allocator_card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(
                            "memory-trim",
                            if trimming {
                                "正在归还…"
                            } else {
                                "归还空闲内存"
                            },
                            theme,
                        )
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .loading(trimming)
                        .on_click(cx.listener(|_this, _, _, cx| trim_heap_now(cx))),
                    )
                    .child(
                        Button::new("memory-copy", "复制诊断", theme)
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| copy_report(this, cx))),
                    ),
            );
            if let Ok(status) = TRIM_STATUS.lock()
                && let Some((released, message)) = status.as_ref()
            {
                allocator_card = allocator_card.child(
                    div()
                        .text_size(px(11.0))
                        .line_height(px(17.0))
                        .text_color(if *released {
                            theme.success
                        } else {
                            theme.text_muted
                        })
                        .child(message.clone()),
                );
            }
            allocator_card = allocator_card.child(
                div()
                    .text_size(px(10.5))
                    .line_height(px(16.0))
                    .text_color(theme.text_faint)
                    .child(
                        "glibc 默认为每个线程开一个 arena。本进程启动时已把上限限制为 4 个，\
                         空闲块因此能跨线程复用，而不是各线程各留一份。",
                    ),
            );
        }
        None => {
            allocator_card = allocator_card.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child("分配器统计仅在 Linux + glibc 上可用。"),
            );
        }
    }

    // --- app data --------------------------------------------------------
    let app_card = panel_card(theme, "应用内占用", "文本与图像载荷估算 · 不含结构体开销")
        .child(panel_row(
            theme,
            "会话索引",
            &format!("{} 个（侧栏 + 归档）", app.sessions),
        ))
        .child(panel_row(
            theme,
            "内存中的 transcript",
            &format!(
                "{} 个 · {} 条消息 · {}",
                app.loaded_transcripts,
                app.transcript_messages,
                format_bytes(app.transcript_bytes)
            ),
        ))
        .child(panel_row(
            theme,
            "聊天流快照",
            &format!(
                "{} 项 · 文本 {} · 工具结果 {} 个 / {}",
                app.chat_items,
                format_bytes(app.chat_bytes),
                app.chat_tool_results,
                format_bytes(app.chat_tool_result_bytes)
            ),
        ))
        .child(panel_row(
            theme,
            "Markdown 缓存",
            &format!(
                "文档 {} 篇 / {} · 分块 {} 块 / {}",
                app.markdown_documents,
                format_bytes(app.markdown_document_bytes),
                app.markdown_chunks,
                format_bytes(app.markdown_chunk_bytes)
            ),
        ))
        .child(panel_row(
            theme,
            "图片缓存",
            &format!(
                "{} 张 · {}（压缩字节）",
                app.decoded_images,
                format_bytes(app.decoded_image_bytes)
            ),
        ))
        .child(panel_row(
            theme,
            "子代理",
            &format!(
                "{} 个任务 · 子会话 {}（已含在 transcript）",
                app.subagents,
                format_bytes(app.subagent_bytes)
            ),
        ))
        .child(panel_row(
            theme,
            "后台运行",
            &format!("{} 个会话", app.background_runs),
        ))
        .child(
            div()
                .text_size(px(10.5))
                .line_height(px(16.0))
                .text_color(theme.text_faint)
                .child(
                    "聊天快照是虚拟列表自持的 Message / ToolResult 副本；Markdown 缓存是解析后\
                     的 IR。两者都会在重建或切换会话时替换，不会无限增长。",
                ),
        );

    let header = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .pb(px(4.0))
        .child(
            div()
                .text_size(px(17.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("内存"),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(theme.text_muted)
                .child("定位 RSS 的构成：应用数据、分配器保留的空闲页，以及其余驱动/字体/共享库"),
        );

    div()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(720.0))
                .p_6()
                .flex()
                .flex_col()
                .gap(px(20.0))
                .child(header)
                .child(process_card)
                .child(allocator_card)
                .child(app_card),
        )
        .into_any_element()
}

/// Keep the page live while it is visible. The ticker stops on its own once
/// `render_panel` has not run for a couple of seconds (section switched or
/// settings closed), so leaving the page does not leave a timer running.
fn ensure_ticker(cx: &mut Context<AppView>) {
    if PANEL_TICKER.swap(true, Ordering::SeqCst) {
        return;
    }
    cx.spawn(async move |weak, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            let recently_visible = LAST_RENDER
                .lock()
                .ok()
                .and_then(|last| *last)
                .is_some_and(|last| last.elapsed() < Duration::from_millis(2500));
            if !recently_visible {
                PANEL_TICKER.store(false, Ordering::SeqCst);
                break;
            }
            let keep = weak
                .update(cx, |_, cx| {
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep {
                PANEL_TICKER.store(false, Ordering::SeqCst);
                break;
            }
        }
    })
    .detach();
}

fn cached_app_memory(view: &AppView) -> AppMemory {
    let now = Instant::now();
    if let Ok(cache) = PANEL_SNAPSHOT.lock()
        && let Some((sampled_at, snapshot)) = *cache
        && now.duration_since(sampled_at) < Duration::from_millis(900)
    {
        return snapshot;
    }
    let snapshot = app_memory(view);
    if let Ok(mut cache) = PANEL_SNAPSHOT.lock() {
        *cache = Some((now, snapshot));
    }
    snapshot
}

fn trim_heap_now(cx: &mut Context<AppView>) {
    if TRIMMING.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Ok(mut status) = TRIM_STATUS.lock() {
        *status = None;
    }
    cx.notify();
    cx.spawn(async move |weak, cx| {
        let (before, released, after) = cx
            .background_executor()
            .spawn(async move {
                let before = sample_process().map_or(0, |process| process.rss_bytes);
                let released = trim_heap();
                let after = sample_process().map_or(before, |process| process.rss_bytes);
                (before, released, after)
            })
            .await;
        let updated = weak.update(cx, |_, cx| {
            TRIMMING.store(false, Ordering::SeqCst);
            if let Ok(mut cache) = PANEL_SNAPSHOT.lock() {
                *cache = None;
            }
            let message = if released && after < before {
                format!(
                    "已向系统归还 {}（RSS {} → {}）。",
                    format_bytes(before - after),
                    format_bytes(before),
                    format_bytes(after)
                )
            } else if released {
                format!(
                    "已释放空闲页，但其他线程仍在分配（当前 RSS {}）。",
                    format_bytes(after)
                )
            } else {
                "没有可直接归还的顶部空闲页：空闲块被仍在使用的分配隔开。重启后新的 arena \
                 上限会阻止再次累积。"
                    .to_string()
            };
            if let Ok(mut status) = TRIM_STATUS.lock() {
                *status = Some((released, message));
            }
            cx.notify();
        });
        if updated.is_err() {
            TRIMMING.store(false, Ordering::SeqCst);
        }
    })
    .detach();
}

fn copy_report(view: &AppView, cx: &mut Context<AppView>) {
    let report = format_report(sample_process(), sample_heap(), &cached_app_memory(view));
    cx.write_to_clipboard(ClipboardItem::new_string(report));
    if let Ok(mut status) = TRIM_STATUS.lock() {
        *status = Some((true, "诊断信息已复制到剪贴板。".to_string()));
    }
    cx.notify();
}

fn panel_card(theme: &Theme, title: &str, subtitle: &str) -> gpui::Div {
    div()
        .w_full()
        .rounded(px(14.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .p(px(20.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(subtitle.to_string()),
                ),
        )
}

fn panel_row(theme: &Theme, label: &str, value: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .child(
            div()
                .max_w(px(380.0))
                .truncate()
                .font_family(theme.mono_font())
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(value.to_string()),
        )
        .into_any_element()
}

fn metrics(theme: &Theme, entries: &[(&str, String, Hsla)]) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .items_start()
        .gap_x(px(18.0))
        .gap_y(px(10.0))
        .children(entries.iter().map(|(label, value, tone)| {
            div()
                .flex_1()
                .min_w(px(120.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(*tone)
                        .child(value.clone()),
                )
                .into_any_element()
        }))
        .into_any_element()
}

fn memory_bar(theme: &Theme, segments: &[(f32, Hsla, &str)]) -> AnyElement {
    let mut bar = div()
        .flex()
        .w_full()
        .h(px(9.0))
        .rounded(px(999.0))
        .overflow_hidden()
        .bg(theme.surface_alt);
    for (fraction, color, _) in segments {
        if *fraction > 0.0 {
            bar = bar.child(div().h_full().w(relative(*fraction)).bg(*color));
        }
    }
    let legend = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(12.0))
        .gap_y(px(4.0))
        .children(
            segments
                .iter()
                .filter(|(fraction, _, _)| *fraction > 0.0)
                .map(|(_, color, label)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(7.0)).rounded(px(999.0)).bg(*color))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child((*label).to_string()),
                        )
                        .into_any_element()
                }),
        );
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bar)
        .child(legend)
        .into_any_element()
}

/// Plain-text diagnostic report for the "复制诊断" button: one block per layer
/// so it can be pasted into an issue without screenshots.
pub(crate) fn format_report(
    process: Option<ProcessMemory>,
    heap: Option<HeapMemory>,
    app: &AppMemory,
) -> String {
    use crate::attachments::format_bytes;

    let mut report = String::from("EffiBuddy memory report\n");
    match process {
        Some(process) => {
            report.push_str(&format!(
                "\n[process]\nrss={} peak={} vm_data={} vm_size={} threads={}\n",
                format_bytes(process.rss_bytes),
                format_bytes(process.peak_rss_bytes),
                format_bytes(process.vm_data_bytes),
                format_bytes(process.vm_size_bytes),
                process.threads,
            ));
        }
        None => report.push_str("\n[process]\nunavailable on this platform\n"),
    }
    match heap {
        Some(heap) => {
            report.push_str(&format!(
                "[allocator]\narena={} in_use={} free_retained={} mmap={}\n",
                format_bytes(heap.arena_bytes),
                format_bytes(heap.in_use_bytes),
                format_bytes(heap.free_bytes),
                format_bytes(heap.mmap_bytes),
            ));
        }
        None => report.push_str("[allocator]\nunavailable (Linux + glibc only)\n"),
    }
    report.push_str(&format!(
        "\n[app]\nsessions={} loaded_transcripts={} messages={} transcript={}\n\
         chat_items={} chat={} tool_results={} tool_result_bytes={}\n\
         markdown_docs={} doc_bytes={} markdown_chunks={} chunk_bytes={}\n\
         decoded_images={} image_bytes={} subagents={} subagent_bytes={}\n\
         background_runs={}\n",
        app.sessions,
        app.loaded_transcripts,
        app.transcript_messages,
        format_bytes(app.transcript_bytes),
        app.chat_items,
        format_bytes(app.chat_bytes),
        app.chat_tool_results,
        format_bytes(app.chat_tool_result_bytes),
        app.markdown_documents,
        format_bytes(app.markdown_document_bytes),
        app.markdown_chunks,
        format_bytes(app.markdown_chunk_bytes),
        app.decoded_images,
        format_bytes(app.decoded_image_bytes),
        app.subagents,
        format_bytes(app.subagent_bytes),
        app.background_runs,
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_status_fields() {
        let raw = "Name:\teffibuddy\nVmSize:\t 3126732 kB\nVmRSS:\t 1360308 kB\n\
                   VmData:\t 1391836 kB\nVmHWM:\t 1368116 kB\nThreads:\t47\n";
        let memory = parse_status(raw).expect("parsed status");
        assert_eq!(memory.rss_bytes, 1_360_308 * 1024);
        assert_eq!(memory.peak_rss_bytes, 1_368_116 * 1024);
        assert_eq!(memory.vm_data_bytes, 1_391_836 * 1024);
        assert_eq!(memory.vm_size_bytes, 3_126_732 * 1024);
        assert_eq!(memory.threads, 47);
    }

    #[test]
    fn json_bytes_walks_nested_values_without_allocating() {
        let value = serde_json::json!({
            "path": "src/main.rs",
            "options": {"recursive": true, "depth": 3},
            "items": ["a", "bb"]
        });
        let size = json_bytes(&value);
        // Keys and strings are counted; scalars/containers add their overhead.
        assert!(size >= "path".len() as u64 + "src/main.rs".len() as u64);
        assert!(size < 1024);
    }

    #[test]
    fn message_bytes_counts_text_reasoning_and_tool_output() {
        let mut message = Message::user("hello");
        message.push_reasoning("thinking hard");
        assert!(message_bytes(&message) >= ("hello".len() + "thinking hard".len()) as u64);
        let with_tool = Message::from_tool_result(effibuddy_agent_core::ToolResult::success(
            &effibuddy_agent_core::ToolCall::new("read_file", serde_json::json!({"path": "a"})),
            "x".repeat(100),
        ));
        assert!(message_bytes(&with_tool) >= 100);
    }
}
