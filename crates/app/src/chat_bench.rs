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
use std::time::Instant;

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
        }
    }

    fn frame(&mut self, cx: &mut gpui::VisualTestContext) -> FrameCost {
        self.chat.tick_scroll();
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
