//! 左栏（`#session-list`）的排版回归守卫。
//!
//! 用户报告：项目（文件夹）下的会话一多，整列会话行就被压扁，旁边的区块跟
//! 着变形，滚动条却算不出可滚范围。原因是这些行直接挂在外层滚动列的 flex 布局
//! 里——外层列高度固定，内容超出一屏时 taffy 按 `flex-shrink: 1` 把每一行压到
//! 最小内容高度（30px 的会话行只剩 19.5px），行高和滚动范围一起塌掉。
//!
//! 修复：每段子项先收进一层 `flex_none` 容器（[`sidebar_items_container`]），
//! 行本身也 `flex_none`，容器高度恒等于内容高度，溢出交给外层滚动列计算。
//! 这里的测试直接排版真实的容器构造函数，锁住三件事：行高不变、内容确实溢出、
//! 可滚范围 = 内容 − 一屏（旧结构下会是 264.5px 而不是 476px）。

use gpui::{
    AppContext as _, Context, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle,
    Stateful, StatefulInteractiveElement, Styled, Window, div, point, px, size,
};

use crate::app::sidebar_items_container;

const VIEWPORT_W: f32 = 272.0;
const VIEWPORT_H: f32 = 200.0;
/// 会话行高度（与 `sidebar_session_row` 一致）。
const ROW_H: f32 = 30.0;
/// 行间距（`#session-list` 与子项容器都用 `gap_1`）。
const GAP: f32 = 4.0;
const ROWS: usize = 20;

/// 复刻侧栏的一条会话行：固定 30px、横向 flex、内部只有一个 12px 文本。
fn session_row(index: usize) -> Stateful<gpui::Div> {
    let mut row = div()
        .id(("row", index))
        .flex()
        .flex_none()
        .items_center()
        .h(px(ROW_H))
        .w_full()
        .child(div().text_size(px(12.0)).child(format!("会话 {index}")));
    if index == 0 {
        row = row.debug_selector(|| "row-first".to_string());
    }
    if index == ROWS - 1 {
        row = row.debug_selector(|| "row-last".to_string());
    }
    row
}

struct Harness {
    handle: ScrollHandle,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 与 `render_sidebar` 里的 `#session-list` 同款：固定高度 + 纵向滚动列。
        div()
            .id("session-list")
            .w(px(VIEWPORT_W))
            .h(px(VIEWPORT_H))
            .overflow_y_scroll()
            .track_scroll(&self.handle)
            .flex()
            .flex_col()
            .gap(px(GAP))
            .child(sidebar_items_container(
                "sidebar-items-project".into(),
                (0..ROWS).map(|index| session_row(index).into_any_element()).collect(),
            ))
    }
}

#[gpui::test]
fn folder_items_keep_their_height_in_the_sidebar_scroller(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let handle = ScrollHandle::new();
    let tracked = handle.clone();
    cx.draw(
        point(px(0.0), px(0.0)),
        size(px(VIEWPORT_W), px(VIEWPORT_H)),
        move |_, cx| {
            cx.new(|_| Harness {
                handle: tracked.clone(),
            })
        },
    );

    let first = cx
        .debug_bounds("row-first")
        .expect("第一条会话行没有排版出来");
    let last = cx
        .debug_bounds("row-last")
        .expect("最后一条会话行没有排版出来");

    assert!(
        (f32::from(first.size.height) - ROW_H).abs() < 0.5,
        "首行被压扁：高 {}px，应为 {ROW_H}px",
        f32::from(first.size.height)
    );
    assert!(
        (f32::from(last.size.height) - ROW_H).abs() < 0.5,
        "末行被压扁：高 {}px，应为 {ROW_H}px",
        f32::from(last.size.height)
    );

    // 溢出必须体现在位置而不是高度上：末行整体排到视口下方。
    let expected_top = (ROWS - 1) as f32 * (ROW_H + GAP);
    assert!(
        (f32::from(last.origin.y) - expected_top).abs() < 1.0,
        "末行起点 {}px，应为内容总高推算的 {expected_top}px（行被压缩了？）",
        f32::from(last.origin.y)
    );
    assert!(
        f32::from(last.origin.y) > VIEWPORT_H,
        "内容没有溢出视口（{}px vs {VIEWPORT_H}px）：滚动范围会塌成 0",
        f32::from(last.origin.y)
    );

    // 关键一条：外层滚动列必须把容器算成「内容高度」，可滚范围 = 内容 − 一屏。
    // 容器被压缩时行位置看着还对（子项溢出容器），但滚动范围会塌，这才是用户
    // 说的「挤得变形、还滚不动」。
    let content_h = ROWS as f32 * ROW_H + (ROWS as f32 - 1.0) * GAP;
    let max_offset = f32::from(handle.max_offset().height);
    assert!(
        (max_offset - (content_h - VIEWPORT_H)).abs() < 1.0,
        "可滚范围 {max_offset:.1}px，应为 {:.1}px（内容 {content_h:.1}px）",
        content_h - VIEWPORT_H
    );
}
