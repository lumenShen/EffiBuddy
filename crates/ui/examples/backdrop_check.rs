//! Manual check for `backdrop_filter`: run it, then look at the window.
//!
//! `cargo run --example backdrop_check -p effibuddy-ui`
//!
//! Expect: a colourful backdrop of stripes and blocks, a perfectly crisp
//! version of the same thing inside a frosted panel in the middle, and crisp
//! text on top of the glass. If the panel looks like unblurred content, or the
//! window is black, the backdrop pass is broken.

use gpui::{
    App, Application, Bounds, Context, Hsla, Render, Window, WindowBounds, WindowOptions, div, hsla,
    prelude::*, px, rgb, size,
};

struct BackdropCheck {
    ticks: u32,
}

impl Render for BackdropCheck {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.ticks = self.ticks.wrapping_add(1);

        // A busy backdrop: hard-edged stripes, colour blocks and a bright band,
        // i.e. detail that *must* disappear inside the glass panel.
        let mut backdrop = div().absolute().inset_0().flex().flex_row();
        for index in 0..60 {
            let hue = (index as f32 * 360.0 / 60.0) / 360.0;
            let dark = index % 2 == 0;
            backdrop = backdrop.child(
                div()
                    .flex_1()
                    .h_full()
                    .bg(hsla(hue, 0.85, if dark { 0.45 } else { 0.62 }, 1.0)),
            );
        }

        div()
            .relative()
            .size_full()
            .bg(rgb(0x101318))
            .child(backdrop)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .absolute()
                            .top(px(80.0))
                            .left(px(60.0))
                            .w(px(520.0))
                            .h(px(90.0))
                            .bg(rgb(0xffffff)),
                    )
                    .child(
                        div()
                            .absolute()
                            .bottom(px(70.0))
                            .right(px(60.0))
                            .w(px(360.0))
                            .h(px(140.0))
                            .bg(rgb(0x0a0c10)),
                    ),
            )
            // The glass panel. Everything above is inside the backdrop; the
            // text below is drawn on top and must stay crisp.
            .child(
                div()
                    .absolute()
                    .left(px(180.0))
                    .top(px(120.0))
                    .w(px(640.0))
                    .h(px(420.0))
                    .rounded(px(18.0))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 0.25))
                    .backdrop_filter(px(28.0), Hsla::from(hsla(0.0, 0.0, 1.0, 0.12)))
                    .child(
                        div()
                            .p(px(24.0))
                            .text_color(rgb(0xffffff))
                            .text_2xl()
                            .child("backdrop_filter(px(28.0), white 12%)"),
                    ),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.0), px(680.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| BackdropCheck { ticks: 0 })
            },
        )
        .unwrap();

        cx.activate(true);
    });
}
