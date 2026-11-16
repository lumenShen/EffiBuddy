//! 消息流虚拟列表的滚动几何回归守卫。
//!
//! 这些测试在无窗口的 GPUI 环境里驱动真实的 `ChatList`：先排版出稳定的历史，
//! 再模拟流式追加 / 用户停在半途，检查「可滚动范围」与「行高」是否被追加的
//! 行破坏。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use effibuddy_agent_core::{Message, SessionState};
use effibuddy_ui::Theme;
use gpui::{
    AppContext as _, IntoElement, ListOffset, Render, Styled, WeakEntity, Window, div, list, point,
    px, size,
};

use crate::app::AppView;
use crate::chat::{ChatList, ChatViewState, RunSnapshot, render_chat_item};

const VIEWPORT_W: f32 = 900.0;
const VIEWPORT_H: f32 = 600.0;

struct Harness {
    state: gpui::ListState,
    view: Rc<ChatViewState>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let view = Rc::clone(&self.view);
        let entity = WeakEntity::<AppView>::new_invalid();
        let theme = Theme::light();
        let count = view.slots.len();
        list(self.state.clone(), move |index: usize, _, _| {
            if index >= count {
                return div().into_any_element();
            }
            render_chat_item(index, &view, &theme, &entity)
        })
        .w_full()
        .h_full()
    }
}

/// 逐帧驱动器，复刻 `render_chat` 的帧序：
/// `tick_scroll` → `tick` → `rebuild` → `sync_scroll` → `prepare_frame` → 排版。
struct Sim {
    chat: ChatList,
    session: SessionState,
    run: RunSnapshot,
    expanded_reasoning: HashSet<String>,
    expanded_tools: HashSet<String>,
    expanded_turns: HashSet<String>,
    collapsed_turns: HashSet<String>,
    agents: HashMap<String, crate::chat::AgentCard>,
}

impl Sim {
    fn new(name: &str) -> Self {
        Self {
            chat: ChatList::new(),
            session: SessionState::new(std::env::temp_dir(), name),
            run: RunSnapshot::default(),
            expanded_reasoning: HashSet::new(),
            expanded_tools: HashSet::new(),
            expanded_turns: HashSet::new(),
            collapsed_turns: HashSet::new(),
            agents: HashMap::new(),
        }
    }

    fn frame(&mut self, cx: &mut gpui::VisualTestContext) {
        self.chat.tick_scroll();
        self.chat.tick();
        self.chat.sync_scroll();
        let entity = WeakEntity::<AppView>::new_invalid();
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
        let state = self.chat.list_state.clone();
        cx.draw(
            point(px(0.), px(0.)),
            size(px(VIEWPORT_W), px(VIEWPORT_H)),
            move |_, cx| cx.new(|_| Harness { state, view }),
        );
    }

    fn settle(&mut self, cx: &mut gpui::VisualTestContext, frames: usize) {
        for _ in 0..frames {
            self.frame(cx);
        }
    }

    fn offset(&self) -> f32 {
        -f32::from(self.chat.list_state.scroll_px_offset_for_scrollbar().y)
    }

    fn max_offset(&self) -> f32 {
        f32::from(self.chat.list_state.max_offset_for_scrollbar().height)
    }

    /// 布局里所有行的估算高度之和（= 列表认为的内容总高）。
    fn content_height(&self) -> f32 {
        self.chat
            .list_state
            .debug_rows()
            .iter()
            .map(|(_, height, _)| *height)
            .sum()
    }

    fn zero_height_rows(&self) -> usize {
        self.chat
            .list_state
            .debug_rows()
            .iter()
            .filter(|(_, height, _)| *height <= 0.01)
            .count()
    }

    fn rows(&self) -> usize {
        self.chat.list_state.item_count()
    }

    /// 逐帧推进打字机，直到这次流式增量全部揭示出来。
    fn drain_reveal(&mut self, cx: &mut gpui::VisualTestContext) {
        for _ in 0..4000 {
            let more = self.chat.tick();
            self.frame(cx);
            if !more {
                break;
            }
        }
    }
}

fn paragraph(turn: usize, index: usize) -> String {
    format!(
        "第 {turn} 回合第 {index} 段：虚拟列表只排版视口附近的行，已定稿的块在帧间复用高度，\
         因此滚动的成本与历史长度基本无关。\n\n"
    )
}

fn build_history(sim: &mut Sim, turns: usize, paragraphs: usize) {
    for turn in 0..turns {
        sim.session
            .push_message(Message::user(format!("第 {turn} 个问题")));
        let mut answer = Message::assistant();
        let mut text = String::new();
        for index in 0..paragraphs {
            text.push_str(&paragraph(turn, index));
        }
        answer.set_text(&text);
        sim.session.push_message(answer);
    }
}

/// 用户报告的 bug：**没有吸附到底部时**消息流来了新行，内容高度丢失，视口被
/// 甩到 `scroll_height = 0` 处。
///
/// 触发条件是 slot key 的稳定前缀从第 0 行断开（活动 turn 是列表第一项，或它
/// 自己就是唯一一项），整张列表于是被重新 splice。此时新行没有「前一行」可继
/// 承高度，旧实现把估算值取成 0，可滚动范围就塌到底了。
/// 冷启动一个长会话（相当于切到一个几千条消息的旧会话）：`ListState::reset`
/// 之后整张表的行都还没画过。旧实现把每一行的估算高度取成 0，于是整条历史只
/// 按「一屏」计算，可滚动范围塌成 ~649px，用户一上滚就被夹到开头。
#[gpui::test]
fn cold_open_long_transcript_has_a_usable_scroll_range(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let mut sim = Sim::new("cold-open");

    // 多回合历史：每回合 user + assistant，正文长到会触发分块渲染。
    for turn in 0..60 {
        sim.session
            .push_message(Message::user(format!("第 {turn} 个问题")));
        let mut answer = Message::assistant();
        let mut text = String::new();
        for index in 0..6 {
            text.push_str(&paragraph(turn, index));
        }
        answer.set_text(&text);
        sim.session.push_message(answer);
    }
    sim.chat.rebuild(&sim.session, &sim.run);
    sim.settle(cx, 2);

    let total = sim.rows();
    let zero = sim.zero_height_rows();
    let content = sim.content_height();
    let max = sim.max_offset();
    let top = sim.chat.list_state.logical_scroll_top().item_ix;
    println!(
        "[cold-open] 行 {total} / 顶部行 {top} / 内容 {content:.0} / 可滚动 {max:.0} / 零高行 {zero}"
    );

    assert!(
        total > 60,
        "历史没有被拆成足够的虚拟行（{total}），复现前提不成立"
    );
    assert_eq!(
        zero, 0,
        "冷启动后有 {zero}/{total} 行高度为 0：整条历史只按一屏算"
    );
    assert!(
        max > VIEWPORT_H * 5.0,
        "冷启动后可滚动范围只有 {max:.0}px，几乎等于一屏（内容 {content:.0}px）"
    );
    // 可滚动范围必须与内容高度成比例，而不是「内容 − 一屏」以外的任意值。
    assert!(
        (max - (content - VIEWPORT_H)).abs() < VIEWPORT_H,
        "可滚动范围 {max:.0}px 与内容 {content:.0}px 不匹配"
    );
    assert!(
        top + 4 >= total,
        "冷启动停在顶部行 {top}/{total}：看到的是开头而不是最新消息"
    );

    // 上滚一屏：必须真的往历史里退，而不是被夹到第 0 行。
    sim.chat.list_state.scroll_by(px(-VIEWPORT_H));
    sim.chat.mark_scrolled();
    sim.settle(cx, 2);
    let after = sim.chat.list_state.logical_scroll_top().item_ix;
    assert!(
        after + 2 <= top,
        "上滚一屏后顶部行只从 {top} 变到 {after}：滚不动或方向相反"
    );
    assert!(
        after > total / 2,
        "上滚一屏就从第 {top} 行跳到第 {after}/{total} 行：行高估算塌了"
    );
}

/// 吸附到底部时，一次大幅长高（长块首次排版、一次大 delta）必须**平滑**追上：
/// 不能在一帧里从旧底部硬切到新底部。
#[gpui::test]
fn pinned_growth_is_followed_smoothly(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let mut sim = Sim::new("pinned-smooth");
    build_history(&mut sim, 4, 4);
    sim.chat.rebuild(&sim.session, &sim.run);
    sim.settle(cx, 3);
    sim.chat.pin_to_bottom();
    sim.settle(cx, 2);
    assert!(sim.chat.pinned(), "应当贴底");
    assert!(sim.offset() + 1.0 >= sim.max_offset(), "贴底时应当已在末尾");

    // 一次性长出一大段内容（远超单帧跟随步长）。走真实的流式路径：文本先进
    // reveal 缓冲，由打字机逐帧揭示，列表随之知道新高度。
    let mut text = String::new();
    for index in 0..40 {
        text.push_str(&paragraph(1, index));
    }
    let before_max = sim.max_offset();
    let before_offset = sim.offset();
    sim.run.running = true;
    sim.session
        .push_message(Message::user("继续，把结论展开写"));
    let mut live = Message::assistant();
    live.set_text("");
    sim.session.push_message(live.clone());
    sim.chat.begin_message(&live.id);
    sim.chat.rebuild(&sim.session, &sim.run);
    sim.frame(cx);

    // 一次性把整段塞进缓冲：下一帧列表就会看到一大截新高度。
    sim.chat.push_text(&live.id, &text);
    let mut grown = Message::assistant();
    grown.set_text(&text);
    *sim.session.messages.last_mut().expect("live answer") = grown;

    sim.frame(cx);
    let grown_max = sim.max_offset();
    let after_growth = sim.offset();
    println!(
        "[growth] max {before_max:.1} → {grown_max:.1} | offset {before_offset:.1} → {after_growth:.1} | pinned {} | follow_sched {}",
        sim.chat.pinned(),
        sim.chat.follow_frame_scheduled
    );
    // 增长当帧之后还有距离，必须靠后续帧缓动追上。
    let grew_eased = after_growth + 0.5 < grown_max;
    let mut frames = 0;
    while sim.offset() + 0.5 < sim.max_offset() && frames < 240 {
        sim.settle(cx, 1);
        frames += 1;
    }
    println!("[growth] 用了 {frames} 帧追到 {}", sim.max_offset());
    assert!(
        grew_eased,
        "增长当帧就已经在底部，没有任何缓动帧：这不是平滑跟随"
    );
    assert!(frames <= 120, "跟随用了 {frames} 帧才到底：太慢，像卡住");
    assert!(
        sim.offset() + 0.5 >= sim.max_offset(),
        "没有追到新的底部：offset {:.1} / max {:.1}",
        sim.offset(),
        sim.max_offset()
    );
    assert!(sim.chat.pinned(), "追到底之后应当仍然贴底");
}

/// 常规流式（每帧只长几个像素）必须仍然**当帧**贴住底部，不能被缓动拖出
/// 一条常驻的空隙。
#[gpui::test]
fn routine_streaming_stays_glued_to_the_bottom(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let mut sim = Sim::new("pinned-glued");
    build_history(&mut sim, 4, 4);
    sim.chat.rebuild(&sim.session, &sim.run);
    sim.settle(cx, 3);
    sim.chat.pin_to_bottom();
    sim.settle(cx, 2);

    sim.run.running = true;
    sim.session.push_message(Message::user("讲下去"));
    let mut live = Message::assistant();
    live.set_text("");
    sim.session.push_message(live.clone());
    sim.chat.begin_message(&live.id);
    sim.chat.rebuild(&sim.session, &sim.run);
    sim.settle(cx, 1);

    let mut text = String::new();
    for index in 0..16 {
        let chunk = paragraph(2, index);
        text.push_str(&chunk);
        let mut grown = Message::assistant();
        grown.set_text(&text);
        *sim.session.messages.last_mut().expect("live answer") = grown;
        sim.chat.push_text(&live.id, &chunk);
        sim.drain_reveal(cx);

        // 打字机排完的每一帧都必须贴住底部，或者最多只差一帧的跟随（大 delta
        // 当帧会刻意留一小段缓动距离，下一帧必须归零）。
        let gap = sim.max_offset() - sim.offset();
        assert!(
            gap <= 64.0,
            "第 {index} 段后排完一帧离底部 {gap:.1}px：跟随掉队了"
        );
        sim.settle(cx, 1);
        let settled_gap = sim.max_offset() - sim.offset();
        assert!(
            settled_gap <= 1.0,
            "第 {index} 段后下一帧仍离底部 {settled_gap:.1}px：跟随没有收敛"
        );
        assert!(sim.chat.pinned(), "第 {index} 段后不再贴底");
    }
}
