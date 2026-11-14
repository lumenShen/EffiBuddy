//! Manual playground for `effibuddy_ui::motion`.
//!
//! Run with:
//!
//! ```text
//! cargo run -p effibuddy-ui --example motion_lab
//! ```
//!
//! The left/right boxes are ordinary GPUI layout. `Swap parent` moves item 0
//! between them; `Reorder` reverses the left column; `Add` inserts a new item
//! with an enter offset. None of these change the child's layout during the
//! animation — `MotionItem` only shifts the paint origin.

use std::time::Duration;

use effibuddy_ui::motion::{MotionController, MotionSpec, motion_item};
use effibuddy_ui::{Theme, radius};
use gpui::{
    AnyElement, App, Application, Bounds, Context, IntoElement, Render, SharedString, Window,
    WindowBounds, WindowOptions, div, point, prelude::*, px, size,
};

struct MotionLab {
    motion: MotionController,
    left: Vec<u32>,
    right: Vec<u32>,
    next_id: u32,
    overlay_item: Option<u32>,
}

impl MotionLab {
    fn new() -> Self {
        Self {
            motion: MotionController::new(),
            left: vec![0, 1],
            right: vec![2],
            next_id: 3,
            overlay_item: None,
        }
    }

    fn swap_parent(&mut self, cx: &mut Context<Self>) {
        self.motion.begin(MotionSpec::LAYOUT);
        if let Some(index) = self.left.iter().position(|id| *id == 0) {
            self.left.remove(index);
            self.right.push(0);
        } else {
            self.right.retain(|id| *id != 0);
            self.left.push(0);
        }
        self.overlay_item = Some(0);
        cx.notify();
    }

    fn reorder(&mut self, cx: &mut Context<Self>) {
        self.motion.begin(MotionSpec::LAYOUT);
        self.left.reverse();
        self.overlay_item = None;
        cx.notify();
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        self.motion.begin(MotionSpec::LAYOUT);
        let id = self.next_id;
        self.next_id += 1;
        self.left.push(id);
        self.overlay_item = None;
        cx.notify();
    }

    fn render_item(&self, id: u32, theme: &Theme) -> AnyElement {
        let motion_id = SharedString::from(format!("item-{id}"));
        let mut card = div()
            .w(px(140.0))
            .h(px(44.0))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.5))
            .text_color(theme.text)
            .child(format!("Item {id}"));

        if let Some(progress) = self.motion.enter_progress(&motion_id) {
            card = card.opacity(progress);
        }

        let mut item =
            motion_item(&self.motion, motion_id, card).enter_offset(point(px(0.0), px(10.0)));
        if self.overlay_item == Some(id) {
            // Cross-parent moves paint above both boxes instead of being
            // clipped by the destination. The layout still belongs to the
            // destination box.
            item = item.overlay_while_animating(10);
        }
        item.into_any_element()
    }

    fn render_box(&self, title: &'static str, items: &[u32], theme: &Theme) -> impl IntoElement {
        div()
            .w(px(188.0))
            .h(px(250.0))
            .p(px(12.0))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_alt)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(title),
            )
            .children(items.iter().map(|id| self.render_item(*id, theme)))
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        on_click: fn(&mut Self, &mut Context<Self>),
    ) -> AnyElement {
        div()
            .id(id)
            .px(px(12.0))
            .py(px(7.0))
            .rounded(px(radius::SM))
            .bg(theme.accent)
            .text_color(theme.on_accent)
            .text_size(px(12.0))
            .cursor_pointer()
            .hover(|style| style.opacity(0.85))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .into_any_element()
    }
}

impl Render for MotionLab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::light();

        div()
            .size_full()
            .bg(theme.bg)
            .p(px(24.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(theme.text)
                            .child("Motion Lab"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(
                                "FLIP reorder, cross-parent swap, enter offset. Layout stays put.",
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(16.0))
                    .child(self.render_box("Inbox", &self.left, &theme))
                    .child(self.render_box("Done", &self.right, &theme)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(self.button("swap-parent", "Swap parent", &theme, cx, Self::swap_parent))
                    .child(self.button("reorder", "Reorder", &theme, cx, Self::reorder))
                    .child(self.button("add", "Add item", &theme, cx, Self::add)),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(500.0), px(430.0)),
                cx,
            ))),
            titlebar: None,
            ..Default::default()
        };

        let handle = cx
            .open_window(options, |_window, cx| cx.new(|_| MotionLab::new()))
            .expect("failed to open the motion lab window");

        // A smoke-test hook: keep swapping parents so the deferred overlay
        // path runs without a pointer. `MOTION_LAB_AUTO=1 cargo run ...`
        if std::env::var_os("MOTION_LAB_AUTO").is_some() {
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(1200))
                        .await;
                    if handle
                        .update(cx, |view, _, cx| view.swap_parent(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        cx.activate(true);
    });
}
