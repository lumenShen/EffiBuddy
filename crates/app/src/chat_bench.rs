//! 消息流性能基准（10k 消息滚动）。
//!
//! 这里构造一份接近真实对话的 transcript——**用户输入 / 思考（reasoning）/
//! 正文（markdown）/ 工具调用 + 工具结果** 四类内容交错——然后用真实的
//! `ChatList`（虚拟化槽位、分块 markdown、前缀 splice）在无窗口的 GPUI 测试
//! 环境里逐帧排版并绘制，测量三类成本：
//!
//! * `rebuild`：结构性重建（打开会话 / 每个工具边界）一次的全量成本；
//! * `prepare_frame`：每帧固定成本（虚拟行计划、slot key 指纹）；
//! * 滚动帧：`draw` 的真实排版 + 绘制成本（含新行 markdown 构建）。
//!
//! 运行方式（必须 release，否则数字没有参考价值）：
//!
//! ```text
//! EFFIBUDDY_BENCH_10K=1 cargo test --release -p effibuddy-app --bin effibuddy \
//!     bench_10k_stream -- --nocapture
//! ```
//!
//! 可选环境变量：`EFFIBUDDY_BENCH_N`（消息数，默认 10000）、
//! `EFFIBUDDY_BENCH_FRAMES`（滚动屏数，默认 600）。
//!
//! 说明：测试环境用 GPUI 的 `NoopTextSystem`（等宽近似），换行宽度与真实字体
//! 有差异，因此「文字排版」成本偏乐观；结构性成本（重建、槽位计划、markdown
//! 块构建）与真实运行一致。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use effibuddy_agent_core::{ContentBlock, Message, SessionState, ToolCall, ToolResult};
use effibuddy_ui::Theme;
use gpui::{
    AppContext as _, IntoElement, ListOffset, Render, Styled, WeakEntity, Window, div, list, point,
    px, size,
};

use crate::app::AppView;
use crate::chat::{
    AgentCard, ChatList, ChatViewState, ESTIMATED_ROW_HEIGHT, RunSnapshot, render_chat_item,
};

const VIEWPORT_W: f32 = 1280.0;
const VIEWPORT_H: f32 = 840.0;
const LONG_BODY_TURNS: usize = 4;

// --- 合成内容 --------------------------------------------------------------

/// 思考块：真实 reasoning 是长段连续文字。
fn reasoning_text(turn: usize) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("我先梳理一下这个问题的约束条件，然后决定用哪条路径。\n");
    for index in 0..4 {
        out.push_str(&format!(
            "第 {index} 步：检查 crates/agent-core 里的第 {turn} 组调用链，确认缓存失效点是否覆盖了流式增量。"
        ));
    }
    out.push_str("总体判断：先量化再动手，避免为看不见的画面把帧跑满。");
    out
}

/// 正文：富 markdown（标题、列表、围栏、表格）。每 `LONG_BODY_TURNS` 个回合
/// 产生一段远超分块阈值的长文，用来覆盖 split（chunk）渲染路径。
fn body_text(turn: usize) -> String {
    let sections = if turn % LONG_BODY_TURNS == 0 { 8 } else { 1 };
    let mut out = String::with_capacity(sections * 420);
    out.push_str(&format!("## 回合 {turn} 的结论\n\n"));
    for section in 0..sections {
        out.push_str(&format!(
            "第 {section} 段：虚拟列表只排版视口附近的行，已定稿的块在帧间复用高度，因此滚动的成本与历史长度基本无关。\n\n"
        ));
        out.push_str(
            "- 要点一：打字机每帧调度一次\n- 要点二：平滑滚动按指数缓动追赶目标\n- 要点三：工具卡片仅在展开时序列化 JSON\n\n",
        );
        out.push_str(&format!(
            "```rust\nfn step_{turn}_{section}(state: &mut State) {{\n    state.advance(160);\n    state.glide();\n}}\n```\n\n"
        ));
    }
    out.push_str("| 指标 | 数值 |\n|---|---|\n| 帧预算 | 8.3ms |\n| 缓动常数 | 85ms |\n");
    out
}

/// 工具结果：一段 grep/read 风格的观察输出。
fn tool_output(turn: usize) -> String {
    let mut out = String::with_capacity(1200);
    for index in 0..12 {
        out.push_str(&format!(
            "crates/app/src/chat.rs:{line}: 回合 {turn} 第 {index} 条匹配（虚拟行 {index}）\n",
            line = 1000 + index
        ));
    }
    out
}

/// 用户输入：短则一行，长则带换行的多段描述。
fn user_prompt(turn: usize) -> String {
    if turn % 3 == 0 {
        format!(
            "帮我看一下第 {turn} 处滚动边界：上下都能滚到内容吗？顺便确认折叠展开后高度缓存没有失真。"
        )
    } else {
        format!(
            "第 {turn} 个问题：\n1. 虚拟化的槽位在流式过程中如何 splice？\n2. 展开态变化会让前缀对齐从哪里断开？"
        )
    }
}

// --- 会话合成 --------------------------------------------------------------

/// 每回合 4 条消息：
/// user → assistant（思考 + 工具调用）→ tool result → assistant（思考 + 正文）。
fn build_session(turns: usize) -> SessionState {
    let mut session = SessionState::new(std::env::temp_dir(), "bench-10k");
    for turn in 0..turns {
        session.push_message(Message::user(user_prompt(turn)));

        let mut plan = Message::assistant();
        plan.push_reasoning(reasoning_text(turn));
        let call = ToolCall::new(
            "read_file",
            serde_json::json!({"path": "crates/app/src/chat.rs"}),
        )
        .with_id(format!("call_{turn}"));
        plan.push_tool_call(call.clone());
        session.push_message(plan);

        let mut result = ToolResult::success(&call, tool_output(turn));
        result.duration_ms = Some(12);
        session.push_message(Message::from_tool_result(result));

        let mut answer = Message::assistant();
        answer.push_reasoning(reasoning_text(turn + 1));
        answer.push_text(body_text(turn));
        session.push_message(answer);
    }
    session
}

// --- 统计 ------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    samples: Vec<f64>,
}

impl Stats {
    fn push(&mut self, value: f64) {
        self.samples.push(value);
    }

    fn summary(&self) -> (f64, f64, f64, f64) {
        if self.samples.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        let at = |fraction: f64| -> f64 {
            let index = ((sorted.len() as f64 * fraction).ceil() as usize)
                .saturating_sub(1)
                .min(sorted.len() - 1);
            sorted[index]
        };
        let average = sorted.iter().sum::<f64>() / sorted.len() as f64;
        (average, at(0.50), at(0.95), sorted[sorted.len() - 1])
    }

    fn line(&self, label: &str) -> String {
        let (avg, p50, p95, max) = self.summary();
        format!(
            "  {label:<24} avg {avg:7.2}ms  p50 {p50:7.2}ms  p95 {p95:7.2}ms  max {max:7.2}ms  (n={})",
            self.samples.len()
        )
    }
}

// --- 渲染夹具 --------------------------------------------------------------

struct Harness {
    state: gpui::ListState,
    view: Rc<ChatViewState>,
    rows: Rc<Cell<usize>>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let view = Rc::clone(&self.view);
        let entity = WeakEntity::<AppView>::new_invalid();
        let theme = Theme::light();
        let count = view.slots.len();
        let rows = self.rows.clone();
        list(self.state.clone(), move |index: usize, _, _| {
            if index >= count {
                return div().into_any_element();
            }
            rows.set(rows.get() + 1);
            render_chat_item(index, &view, &theme, &entity)
        })
        .w_full()
        .h_full()
    }
}

struct Sim {
    chat: ChatList,
    session: SessionState,
    run: RunSnapshot,
    rows: Rc<Cell<usize>>,
    expanded_reasoning: HashSet<String>,
    expanded_tools: HashSet<String>,
    expanded_turns: HashSet<String>,
    collapsed_turns: HashSet<String>,
    agents: HashMap<String, AgentCard>,
    /// Offset the glide moved *this* frame (test diagnostics).
    last_tick_delta: f32,
    /// How much the total content height changed during the frame, and whether
    /// the row count changed (test diagnostics).
    last_max_delta: f32,
    last_count_delta: i64,
}

struct FrameCost {
    prepare_ms: f64,
    draw_ms: f64,
    rows: usize,
}

impl Sim {
    fn new(messages: usize) -> Self {
        let turns = messages / 4;
        Self {
            chat: ChatList::new(),
            session: build_session(turns),
            run: RunSnapshot::default(),
            rows: Rc::new(Cell::new(0)),
            expanded_reasoning: HashSet::new(),
            expanded_tools: HashSet::new(),
            expanded_turns: HashSet::new(),
            collapsed_turns: HashSet::new(),
            agents: HashMap::new(),
            last_tick_delta: 0.0,
            last_max_delta: 0.0,
            last_count_delta: 0,
        }
    }

    fn frame(&mut self, cx: &mut gpui::VisualTestContext) -> FrameCost {
        self.frame_at(cx, Instant::now())
    }

    /// One frame with an injected clock, so the mouse replay below can advance
    /// time at a display rate instead of racing real time (where a tight loop
    /// has `dt == 0` and the glide would never move).
    fn frame_at(&mut self, cx: &mut gpui::VisualTestContext, now: Instant) -> FrameCost {
        let before_tick = -f32::from(self.chat.list_state.scroll_px_offset_for_scrollbar().y);
        let before_max = f32::from(self.chat.list_state.max_offset_for_scrollbar().height);
        let before_count = self.chat.list_state.item_count();
        self.chat.tick_scroll_at(now);
        self.last_tick_delta =
            -f32::from(self.chat.list_state.scroll_px_offset_for_scrollbar().y) - before_tick;
        self.last_max_delta =
            f32::from(self.chat.list_state.max_offset_for_scrollbar().height) - before_max;
        self.chat.tick();
        self.chat.sync_scroll();

        let entity = WeakEntity::<AppView>::new_invalid();
        let started = Instant::now();
        let view = Rc::new(self.chat.prepare_frame(
            self.run.running,
            true,
            &self.expanded_reasoning,
            &self.expanded_tools,
            &self.expanded_turns,
            &self.collapsed_turns,
            &self.agents,
            &entity,
        ));
        let prepare_ms = elapsed_ms(started);

        let state = self.chat.list_state.clone();
        let rows = self.rows.clone();
        self.rows.set(0);
        let started = Instant::now();
        cx.draw(
            point(px(0.), px(0.)),
            size(px(VIEWPORT_W), px(VIEWPORT_H)),
            move |_, cx| cx.new(|_| Harness { state, view, rows }),
        );
        FrameCost {
            prepare_ms,
            draw_ms: elapsed_ms(started),
            rows: self.rows.get(),
        }
    }

    fn rebuild(&mut self) -> f64 {
        let started = Instant::now();
        let session = &self.session;
        let run = &self.run;
        self.chat.rebuild(session, run);
        elapsed_ms(started)
    }

    fn offset(&self) -> f32 {
        -f32::from(self.chat.list_state.scroll_px_offset_for_scrollbar().y)
    }

    fn max_offset(&self) -> f32 {
        f32::from(self.chat.list_state.max_offset_for_scrollbar().height)
    }

    fn measured_rows(&self) -> usize {
        self.chat
            .list_state
            .debug_rows()
            .iter()
            .filter(|(_, _, measured)| *measured)
            .count()
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

// --- 基准 ------------------------------------------------------------------

#[gpui::test]
fn bench_10k_stream(cx: &mut gpui::TestAppContext) {
    if std::env::var("EFFIBUDDY_BENCH_10K").is_err() {
        println!("[bench-10k] 跳过：设置 EFFIBUDDY_BENCH_10K=1 以运行");
        return;
    }
    let messages = env_usize("EFFIBUDDY_BENCH_N", 10_000);
    let scroll_screens = env_usize("EFFIBUDDY_BENCH_FRAMES", 600);
    let cx = cx.add_empty_window();
    let mut sim = Sim::new(messages);
    let turn_total = sim.session.messages.len() / 4;

    println!(
        "\n=== 消息流基准：{messages} 条消息 / {turn_total} 回合 / {:.1}MB 文本 ===",
        transcript_bytes(&sim.session) as f64 / (1024.0 * 1024.0)
    );

    // 1) 冷启动：空列表 → 全量重建（打开一个长会话）。
    let cold = sim.rebuild();
    let first = sim.frame(cx);
    let (items, slots) = sim.chat.trace_counts();
    println!(
        "  冷启动 rebuild        {cold:7.2}ms → items {items} / 虚拟行 {slots}；首帧 prepare {:.2}ms draw {:.2}ms",
        first.prepare_ms, first.draw_ms
    );

    // 2) 空闲帧：无滚动、无变化时的每帧固定成本（prepare_frame 的 O(items) 部分）。
    let mut idle_prepare = Stats::default();
    let mut idle_draw = Stats::default();
    for _ in 0..120 {
        let cost = sim.frame(cx);
        idle_prepare.push(cost.prepare_ms);
        idle_draw.push(cost.draw_ms);
    }
    println!("{}", idle_prepare.line("空闲帧 prepare_frame"));
    println!("{}", idle_draw.line("空闲帧 draw(列表)"));

    // 3) 结构性重建（每个工具边界一次）：追加一条消息后重建，然后回滚，模拟
    //    长历史里的一次工具调用边界。
    let mut append = Stats::default();
    for turn in 0..turn_total {
        let mut message = Message::assistant();
        message.push_text(body_text(turn));
        sim.session.push_message(message);
        append.push(sim.rebuild());
        sim.session.messages.pop();
    }
    println!("{}", append.line("追加消息后 rebuild"));

    // 4) 滚动：从底部逐屏走到顶部，每屏都要重新排版新行。
    sim.chat.pin_to_bottom();
    sim.frame(cx);
    println!(
        "  布局：{} 行 / 已测量 {} 行 / 可滚动 {:.0}px",
        sim.chat.list_state.item_count(),
        sim.measured_rows(),
        sim.max_offset()
    );
    let mut scroll_prepare = Stats::default();
    let mut scroll_draw = Stats::default();
    let mut scroll_rows = Stats::default();
    let mut screens = 0usize;
    let total = sim.chat.list_state.item_count();
    while screens < scroll_screens {
        let ix = total.saturating_sub(4 * (screens + 1));
        sim.chat.list_state.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.0),
        });
        sim.chat.mark_scrolled();
        let cost = sim.frame(cx);
        scroll_prepare.push(cost.prepare_ms);
        scroll_draw.push(cost.draw_ms);
        scroll_rows.push(cost.rows as f64);
        screens += 1;
        if ix == 0 {
            break;
        }
    }
    println!("{}", scroll_prepare.line("滚动帧 prepare_frame"));
    println!("{}", scroll_draw.line("滚动帧 draw(列表)"));
    let (rows_avg, _, _, _) = scroll_rows.summary();
    println!(
        "  滚动覆盖 {screens} 屏 / {total} 行，平均每帧重排 {} 行；终态已测量 {} 行",
        rows_avg.round() as usize,
        sim.measured_rows()
    );
    // 行高估算精度：估算值只影响首帧的滚动条刻度，随测量自愈。这里报告
    // 「已测量行的真实均值」与「首帧估算值」，确认误差量级。
    let rows = sim.chat.list_state.debug_rows();
    let measured: Vec<f32> = rows
        .iter()
        .filter(|(_, _, measured)| *measured)
        .map(|(_, height, _)| *height)
        .collect();
    if !measured.is_empty() {
        let average = measured.iter().sum::<f32>() / measured.len() as f32;
        println!(
            "  行高：已测量 {average:.0}px 均值（首帧估算 {ESTIMATED_ROW_HEIGHT:.0}px，估算总高 {:.0}px）",
            rows.len() as f32 * ESTIMATED_ROW_HEIGHT
        );
    }

    // 5) 长历史里的流式：尾部新消息逐帧揭示（最坏情况的每帧固定成本）。
    sim.chat.pin_to_bottom();
    let mut live = Message::assistant();
    live.set_text("");
    sim.session.push_message(live.clone());
    sim.run.running = true;
    sim.rebuild();
    sim.chat.begin_message(&live.id);
    let mut stream_prepare = Stats::default();
    let mut stream_draw = Stats::default();
    for index in 0..160 {
        sim.chat.push_text(&live.id, &body_text(index));
        let cost = sim.frame(cx);
        stream_prepare.push(cost.prepare_ms);
        stream_draw.push(cost.draw_ms);
    }
    println!("{}", stream_prepare.line("流式帧 prepare_frame"));
    println!("{}", stream_draw.line("流式帧 draw(列表)"));

    // 6) 成本对照：确认「全量重哈希」确实是重建的主要开销之一（重建改用
    //    `Message::revision` 的依据），以及正文/思考拼接复制的成本。
    let mut hash_stats = Stats::default();
    let mut copy_stats = Stats::default();
    for _ in 0..40 {
        let started = Instant::now();
        let mut accumulator = 0u64;
        for message in &sim.session.messages {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            message.id.hash(&mut hasher);
            for block in &message.blocks {
                match block {
                    ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                        text.hash(&mut hasher)
                    }
                    ContentBlock::ToolResult { result } => result.content.len().hash(&mut hasher),
                    _ => {}
                }
            }
            accumulator ^= hasher.finish();
        }
        hash_stats.push(elapsed_ms(started));
        std::hint::black_box(accumulator);

        let started = Instant::now();
        let mut total = 0usize;
        for message in &sim.session.messages {
            total += message.text().len() + message.reasoning().len();
        }
        copy_stats.push(elapsed_ms(started));
        std::hint::black_box(total);
    }
    println!("{}", hash_stats.line("  全量文本哈希(旧签名)"));
    println!("{}", copy_stats.line("  正文/思考拼接复制"));

    let (items, slots) = sim.chat.trace_counts();
    println!("  终态：items {items} / 虚拟行 {slots}\n");
}

fn transcript_bytes(session: &SessionState) -> usize {
    session
        .messages
        .iter()
        .map(|message| {
            message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
                    ContentBlock::ToolCall { call } => call.name.len() + call.id.len(),
                    ContentBlock::ToolResult { result } => result.content.len(),
                })
                .sum::<usize>()
        })
        .sum()
}

// --- 回归守卫 --------------------------------------------------------------

/// 冷启动（`ListState::reset` 之后第一次布局）有两个必须同时成立的属性：
///
/// 1. 行高有合理估算：否则整条历史的估算高度是 0，滚动范围塌成「一屏」，
///    滚轮一向上就跳到会话开头，中间的几千条消息永远看不到。
/// 2. 跟随末尾：`reset` 会清掉逻辑滚动位置，长会话打开后应当停在**最新**的
///    消息上，而不是几千条消息之前的开头。
///
/// 这两条在 10k 消息实测里都曾失效：可滚动范围只有 649px（约一屏），顶部行是 0。
#[gpui::test]
fn cold_open_transcript_opens_at_the_tail_with_a_usable_scroll_range(
    cx: &mut gpui::TestAppContext,
) {
    let cx = cx.add_empty_window();
    let mut sim = Sim::new(400);
    sim.rebuild();
    // `reset` 之后的末尾补偿要等一次真实布局拿到行高才生效，因此跑两帧。
    sim.frame(cx);
    sim.frame(cx);
    let total = sim.chat.list_state.item_count();
    let top_ix = sim.chat.list_state.logical_scroll_top().item_ix;
    let rows = sim.chat.list_state.debug_rows();
    let zero = rows.iter().filter(|(_, height, _)| *height <= 0.01).count();
    println!(
        "[cold-open] 行 {total} / 顶部行 {top_ix} / max {:.0} / 零高行 {zero}",
        sim.max_offset()
    );

    assert_eq!(zero, 0, "有 {zero} 行的估算高度为 0：滚动范围不可信");
    assert!(
        sim.max_offset() > 10.0 * VIEWPORT_H,
        "长会话的可滚动范围只有 {:.0}px：历史行高估算缺失",
        sim.max_offset()
    );
    assert!(
        top_ix + 4 >= total,
        "冷启动停在顶部行 {top_ix}/{total}：打开长会话看到的是开头而不是最新消息"
    );

    // 向上退进历史：必须按像素精确移动，且不能一步跳到开头——后者正是行高
    // 估算塌成 0 时的表现（整条历史只有一屏高，任何上滚都被夹回第 0 行）。
    // 这里直接驱动 list 的滚动原语：`ScrollGlide` 是按真实时间指数缓动的，
    // 紧凑测试循环里每帧 dt≈0，测不出位移。
    let before_ix = sim.chat.list_state.logical_scroll_top().item_ix;
    let before_offset = sim.offset();
    sim.chat.list_state.scroll_by(px(-VIEWPORT_H));
    sim.chat.mark_scrolled();
    sim.frame(cx);
    let after_ix = sim.chat.list_state.logical_scroll_top().item_ix;
    let moved = before_offset - sim.offset();
    println!("[cold-open] 上滚一屏 {moved:.0}px：顶部行 {before_ix} → {after_ix}（共 {total} 行）");
    // 不要求像素级精确：上滚时新行会被真实测量，内容总高度随之修正，
    // 偏移读数会有几十像素的量级差。这里只要求「确实退了一大段」。
    assert!(
        moved > VIEWPORT_H * 0.5,
        "上滚一屏实际只移动了 {moved:.0}px：滚动刻度与行高估算不一致"
    );
    assert!(
        after_ix < before_ix,
        "上滚一屏后顶部行没有前移（{before_ix} → {after_ix}）：历史滚不动"
    );
    assert!(
        after_ix > total / 2,
        "上滚一屏就从第 {before_ix} 行跳到第 {after_ix}/{total} 行：滚动刻度失真"
    );
}

// --- 滚轮累加回归 ----------------------------------------------------------

/// 连续滚 10 格滚轮，合计应当移动约 10 格的距离。
///
/// 旧实现里 `ScrollGlide::tick` 把「读回的偏移与内部记录的位置不一致」一律
/// 当成外部滚动（滚动条拖拽 / 程序化跳转），于是把 `target` 重置成当前帧的
/// 偏移。结果是每一格滚轮都从「现在这一帧的位置」重新起步，滚 10 格只走 1 格
/// 的距离——手感上就是「使劲滚却滚不动」。
#[gpui::test]
fn wheel_notches_accumulate_instead_of_restarting(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let mut sim = Sim::new(2000);
    sim.rebuild();
    sim.frame(cx);
    sim.frame(cx);

    // 对齐到会话中段，留出上下两个方向的余量。
    let total = sim.chat.list_state.item_count();
    sim.chat.list_state.scroll_to(ListOffset {
        item_ix: total / 2,
        offset_in_item: px(0.0),
    });
    sim.chat.mark_scrolled();
    sim.frame(cx);
    sim.frame(cx);

    const NOTCHES: usize = 10;
    const NOTCH_PX: f32 = 20.0;
    let before = sim.offset();
    for _ in 0..NOTCHES {
        sim.chat.wheel(NOTCH_PX);
    }
    let mut frames = 0;
    while sim.chat.tick_scroll() && frames < 4000 {
        sim.frame(cx);
        frames += 1;
    }
    sim.frame(cx);

    let travelled = before - sim.offset();
    let expected = NOTCHES as f32 * NOTCH_PX;
    println!(
        "[wheel] {NOTCHES} 格 × {NOTCH_PX}px → 实际 {travelled:.1}px（期望 ≈{expected:.0}px，{frames} 帧）"
    );
    assert!(
        travelled > expected * 0.8,
        "滚了 {NOTCHES} 格只移动了 {travelled:.1}px（期望 ≈{expected:.0}px）：滚轮增量被丢掉了"
    );
}

// --- 鼠标滚动行为模拟 ------------------------------------------------------

/// Deterministic xorshift, so a failing seed can be replayed exactly.
struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 32) as u32) ^ (x as u32)
    }

    fn range(&mut self, low: u32, high: u32) -> u32 {
        low + self.next_u32() % (high - low + 1)
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.next_u32() % 100 < percent
    }
}

/// One wheel event as the platform would deliver it.
struct WheelEvent {
    at_ms: f64,
    /// GPUI convention: positive scrolls toward the start.
    pixel_y: f32,
}

/// A gesture: a burst of notches followed by a pause, the way a hand on a wheel
/// actually produces them — irregular gaps, occasional double-size Delts.
fn wheel_gesture(rng: &mut Rng, start_ms: f64, toward_start: bool) -> Vec<WheelEvent> {
    let notches = rng.range(1, 12);
    // Fast flicks arrive every few ms, slow drags every ~60ms.
    let (gap_low, gap_high) = if rng.chance(60) { (2, 18) } else { (30, 90) };
    let mut at = start_ms;
    let mut events = Vec::with_capacity(notches as usize);
    for _ in 0..notches {
        // Magnitude jitter too: some platforms send two lines per detent.
        let magnitude = 20.0 * if rng.chance(20) { 2.0 } else { 1.0 };
        events.push(WheelEvent {
            at_ms: at,
            pixel_y: if toward_start { magnitude } else { -magnitude },
        });
        at += rng.range(gap_low, gap_high) as f64;
    }
    events
}

/// What a gesture is supposed to do, and how the transcript actually behaved.
#[derive(Debug, Default)]
struct GestureReport {
    expected_px: f32,
    travelled_px: f32,
    /// Frames where the offset moved *against* the gesture.
    backward_jumps: usize,
    worst_backward_px: f32,
    /// Largest single-frame move; a teleport shows up here.
    peak_frame_px: f32,
    /// Frames between the first wheel event and the first pixel of motion.
    latency_frames: usize,
    /// Milliseconds from the last wheel event until the offset stops moving.
    settle_ms: f64,
    /// How much of the requested distance had been delivered when the hand
    /// stopped. Low values read as "laggy/blocked".
    delivered_at_gesture_end: f32,
}

impl GestureReport {
    fn capture_ratio(&self) -> f32 {
        if self.expected_px.abs() < 1.0 {
            1.0
        } else {
            self.travelled_px / self.expected_px
        }
    }
}

/// Replay a jittered mouse-wheel gesture against the real `ChatList` and
/// compare the resulting trajectory with the behaviour the user expects:
/// every notch counts, motion follows the hand, nothing moves backwards, and
/// the view settles promptly.
///
/// This is the regression that keeps coming back: the glide used to reset its
/// target whenever the read-back offset disagreed with its internal belief, so
/// a fast flick delivered one notch's distance and felt "stuck". Timing jitter
/// matters because back-to-back events land inside a single frame at 300 Hz —
/// exactly the case where an accumulator that re-bases from the current frame
/// offset loses everything.
/// 可见滚动进度：行锚点 (行号 × 平均行高 + 行内偏移)。
///
/// 不能用像素偏移衡量「用户看到的位移」—— 列表锚定的是行，行以上被测量后像素
/// 坐标系会整体平移，而可见内容并没有动。
fn anchor_progress(chat: &ChatList, mean: f32) -> f32 {
    let top = chat.list_state.logical_scroll_top();
    top.item_ix as f32 * mean + f32::from(top.offset_in_item)
}

/// Average height of the rows the list has measured so far. Captured once and
/// reused as a fixed scale factor: recomputing it per frame would make the
/// anchor metric itself wobble.
fn measured_mean_row_height(chat: &ChatList) -> f32 {
    let mut measured = 0.0f32;
    let mut count = 0.0f32;
    for (_, height, is_measured) in chat.list_state.debug_rows() {
        if is_measured {
            measured += height;
            count += 1.0;
        }
    }
    if count > 0.0 { measured / count } else { 96.0 }
}

/// Replay jittered mouse-wheel gestures against the real `ChatList` and compare
/// the resulting *visible* trajectory with what a user expects: every notch
/// counts, motion starts immediately, follows the hand, never reverses, and
/// settles promptly.
///
/// Jitter matters: back-to-back notches land inside a single frame at 300 Hz,
/// which is exactly the case where a glide that re-bases its target from the
/// current frame offset silently drops the accumulated intent ("使劲滚却滚不动").
#[gpui::test]
fn jittered_wheel_input_matches_the_expected_trajectory(cx: &mut gpui::TestAppContext) {
    const FRAME_MS: f64 = 1000.0 / 300.0;
    const SETTLE_LIMIT_MS: f64 = 1500.0;
    let cx = cx.add_empty_window();

    let mut sim = Sim::new(4000);
    sim.rebuild();
    sim.frame(cx);
    sim.frame(cx);

    let total = sim.chat.list_state.item_count();
    sim.chat.list_state.scroll_to(ListOffset {
        item_ix: total / 2,
        offset_in_item: px(0.0),
    });
    sim.chat.mark_scrolled();
    sim.frame(cx);

    let mut worst_capture: f32 = 1.0;
    let mut worst_backward_px: f32 = 0.0;
    let mut worst_latency = 0usize;
    let mut worst_settle: f64 = 0.0;
    let mut worst_delivered: f32 = 1.0;
    let mut worst_offset_shift: f32 = 0.0;
    let mut seeds = 0usize;

    for seed in 1..=24u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let toward_start = seed % 2 == 0;
        let events = wheel_gesture(&mut rng, 0.0, toward_start);
        let gesture_end_ms = events.last().expect("non-empty gesture").at_ms;

        let max = f32::from(sim.chat.list_state.max_offset_for_scrollbar().height);
        let input: f32 = events.iter().map(|event| event.pixel_y).sum();
        // 期望：手势总距离，按方向取正。
        let expected_px = input.abs().min(max);

        let mean_row = measured_mean_row_height(&sim.chat);
        let start_anchor = anchor_progress(&sim.chat, mean_row);
        let mut previous_anchor = start_anchor;
        let mut next_event = 0usize;
        let mut now_ms = 0.0f64;
        let mut first_motion_ms: Option<f64> = None;
        let mut last_motion_ms = 0.0f64;
        let mut delivered_at_end: Option<f32> = None;
        let mut worst_backward_here = 0.0f32;
        let mut offset_shift_px = 0.0f32;
        let mut applied_total = 0.0f32;
        let mut frames = 0usize;

        loop {
            while next_event < events.len() && events[next_event].at_ms <= now_ms {
                sim.chat.wheel(events[next_event].pixel_y);
                next_event += 1;
            }
            let base = Instant::now();
            sim.frame_at(cx, base + Duration::from_micros((now_ms * 1000.0) as u64));

            let anchor = anchor_progress(&sim.chat, mean_row);
            // 手势方向为正的「内容位移」由 glide 自己施加的像素决定；像素偏移
            // 还会因为上方行被测量而整体平移，那个平移不是用户看到的移动。
            let applied = if toward_start {
                -sim.last_tick_delta
            } else {
                sim.last_tick_delta
            };
            applied_total += sim.last_tick_delta;
            if applied.abs() > 0.01 {
                if first_motion_ms.is_none() {
                    first_motion_ms = Some(now_ms);
                }
                last_motion_ms = now_ms;
                if applied < -1.0 {
                    worst_backward_here = worst_backward_here.max(-applied);
                }
            }
            // 诊断：像素坐标系里的反向平移（行被测量导致，不代表可见位移）。
            let raw = previous_anchor - anchor;
            let signed = if toward_start { raw } else { -raw };
            if signed < -1.0 {
                offset_shift_px = offset_shift_px.max(-signed);
            }
            previous_anchor = anchor;
            frames += 1;

            if delivered_at_end.is_none()
                && next_event == events.len()
                && now_ms >= gesture_end_ms + 100.0
            {
                delivered_at_end = Some(if expected_px < 1.0 {
                    1.0
                } else {
                    applied_total.abs() / expected_px
                });
            }

            let settled = !sim.chat.glide_is_active() && next_event == events.len();
            if settled {
                break;
            }
            now_ms += FRAME_MS;
            assert!(
                now_ms < SETTLE_LIMIT_MS + gesture_end_ms,
                "seed {seed}: 缓动一直没停（{frames} 帧）"
            );
        }

        let travelled = applied_total.abs();
        let capture = if expected_px < 1.0 {
            1.0
        } else {
            travelled / expected_px
        };
        let latency_frames = match first_motion_ms {
            Some(first) => ((first - events[0].at_ms).max(0.0) / FRAME_MS).round() as usize,
            None => frames,
        };
        let settle_ms = (last_motion_ms - gesture_end_ms).max(0.0);
        let delivered = delivered_at_end.unwrap_or(1.0);

        assert!(
            capture > 0.90,
            "seed {seed}: 意图丢失 —— 期望 {expected_px:.0}px，可见位移 {travelled:.0}px（{:.0}%）",
            capture * 100.0
        );
        assert!(
            worst_backward_here < 4.0,
            "seed {seed}: 可见内容反向跳动 {:.1}px",
            worst_backward_here
        );
        assert!(
            latency_frames <= 3,
            "seed {seed}: 首个事件后 {latency_frames} 帧才有位移"
        );
        assert!(
            delivered > 0.60,
            "seed {seed}: 手势结束时只交付 {:.0}%",
            delivered * 100.0
        );

        worst_capture = worst_capture.min(capture);
        worst_backward_px = worst_backward_px.max(worst_backward_here);
        worst_offset_shift = worst_offset_shift.max(offset_shift_px);
        worst_latency = worst_latency.max(latency_frames);
        worst_settle = worst_settle.max(settle_ms);
        worst_delivered = worst_delivered.min(delivered);
        seeds += 1;
    }

    println!(
        "[mouse] {seeds} 组带 jitter 的手势（300Hz，1–12 格，2–90ms 间隔）最坏情况：\
         意图捕获 {:.1}% / glide 反向 {:.1}px / 首帧延迟 {worst_latency} 帧 / \
         手势末已交付 {:.0}% / 停稳 {:.0}ms / 像素坐标平移(行测量) {:.0}px",
        worst_capture * 100.0,
        worst_backward_px,
        worst_delivered * 100.0,
        worst_settle,
        worst_offset_shift,
    );
}
