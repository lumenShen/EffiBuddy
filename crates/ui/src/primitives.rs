//! Small building blocks: spinner, badge, avatar, card, divider, keycap.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyView, BoxShadow, Div, FontWeight, Hsla, IntoElement, Render,
    RenderOnce, SharedString, Transformation, Window, div, percentage, prelude::*, px, svg,
};

use crate::assets::icons;
use crate::theme::{Theme, alpha_color, radius, text as text_size};

#[derive(IntoElement)]
pub struct Spinner {
    color: Hsla,
    size: f32,
}

impl Spinner {
    pub fn new(color: Hsla, size: f32) -> Self {
        Self { color, size }
    }
}

impl RenderOnce for Spinner {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        svg()
            .path(icons::LOADER)
            .size(px(self.size))
            .text_color(self.color)
            .flex_none()
            .with_animation(
                "effibuddy-spinner",
                Animation::new(Duration::from_millis(900)).repeat(),
                |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

fn badge_colors(theme: &Theme, tone: BadgeTone) -> (Hsla, Hsla) {
    match tone {
        BadgeTone::Neutral => (theme.surface_alt, theme.text_muted),
        BadgeTone::Accent => (theme.accent_soft, theme.accent),
        BadgeTone::Success => (theme.success_soft, theme.success),
        BadgeTone::Warning => (theme.warning_soft, theme.warning),
        BadgeTone::Danger => (theme.danger_soft, theme.danger),
    }
}

pub fn badge(theme: &Theme, label: impl Into<SharedString>, tone: BadgeTone) -> Div {
    let (bg, fg) = badge_colors(theme, tone);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(px(22.0))
        .px(px(8.0))
        .rounded(px(radius::FULL))
        .bg(bg)
        .text_color(fg)
        .text_size(px(text_size::XS))
        .font_weight(FontWeight::MEDIUM)
        .child(label.into())
}

/// Compact badge for dense rows (tool metadata, palette tags, list counters).
pub fn small_badge(theme: &Theme, label: impl Into<SharedString>, tone: BadgeTone) -> Div {
    let (bg, fg) = badge_colors(theme, tone);
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.0))
        .px(px(6.0))
        .rounded(px(radius::XS))
        .bg(bg)
        .text_color(fg)
        .text_size(px(text_size::MICRO))
        .font_weight(FontWeight::MEDIUM)
        .child(label.into())
}

/// Neutral 26px pill used by the composer and popover toolbars.
pub fn chip(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(26.0))
        .px(px(9.0))
        .rounded(px(radius::FULL))
        .bg(theme.surface_alt)
        .text_color(theme.text_muted)
        .text_size(px(text_size::XS))
        .child(label.into())
}

/// A small inverted tooltip view, used by icon-only controls.
pub struct TooltipView {
    theme: Theme,
    label: SharedString,
}

impl Render for TooltipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(radius::XS))
            .bg(self.theme.tooltip_bg)
            .text_color(self.theme.tooltip_fg)
            .text_size(px(text_size::XS))
            .line_height(px(16.0))
            .child(self.label.clone())
    }
}

/// Build a tooltip closure suitable for `div::tooltip`.
pub fn tooltip(
    theme: &Theme,
    label: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut gpui::App) -> AnyView + 'static {
    let theme = theme.clone();
    let label = label.into();
    move |_window, cx| {
        let theme = theme.clone();
        let label = label.clone();
        cx.new(|_cx| TooltipView { theme, label }).into()
    }
}

pub fn dot(theme: &Theme, color: Hsla, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(radius::FULL))
        .bg(color)
        .border_1()
        .border_color(theme.surface)
}

pub fn avatar(theme: &Theme, name: &str, size: f32) -> Div {
    let initial = name
        .chars()
        .find(|ch| ch.is_alphanumeric())
        .unwrap_or('E')
        .to_uppercase()
        .to_string();
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(radius::FULL))
        .bg(theme.accent_soft)
        .text_color(theme.accent)
        .text_size(px((size * 0.42).max(10.0)))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial)
}

pub fn card(theme: &Theme) -> Div {
    div()
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
        .rounded(px(radius::LG))
}

pub fn elevated_card(theme: &Theme) -> Div {
    div()
        .bg(theme.elevated)
        .border_1()
        .border_color(theme.border)
        .rounded(px(radius::LG))
        .shadow(shadow_float(theme))
}

/// Soft floating shadow used by the composer and future popovers.
pub fn shadow_float(theme: &Theme) -> Vec<BoxShadow> {
    if theme.is_dark() {
        vec![
            BoxShadow {
                color: alpha_color(0x0000008C),
                offset: gpui::point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            },
            BoxShadow {
                color: alpha_color(0x00000073),
                offset: gpui::point(px(0.0), px(10.0)),
                blur_radius: px(30.0),
                spread_radius: px(-6.0),
            },
        ]
    } else {
        vec![
            BoxShadow {
                color: alpha_color(0x0000000F),
                offset: gpui::point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            },
            BoxShadow {
                color: alpha_color(0x00000012),
                offset: gpui::point(px(0.0), px(10.0)),
                blur_radius: px(28.0),
                spread_radius: px(-6.0),
            },
        ]
    }
}

pub fn divider(theme: &Theme) -> Div {
    div().h(px(1.0)).w_full().bg(theme.border)
}

pub fn vertical_divider(theme: &Theme) -> Div {
    div().w(px(1.0)).h_full().bg(theme.border)
}

pub fn kbd(theme: &Theme, key: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .h(px(18.0))
        .px(px(5.0))
        .rounded(px(radius::XS))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.surface_alt)
        .text_color(theme.text_faint)
        .text_size(px(10.0))
        .font_family(theme.mono_font())
        .child(key.into())
}

pub fn section_label(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(text_size::XS))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_faint)
        .child(label.into())
}

/// A 38×22 switch track with a sliding knob. Attach an `on_click` to the
/// returned element; the whole track is the hit target.
pub fn switch(theme: &Theme, on: bool) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .w(px(36.0))
        .h(px(20.0))
        .p(px(2.0))
        .rounded(px(radius::FULL))
        .bg(if on { theme.accent } else { theme.disabled_bg })
        .border_1()
        .border_color(if on {
            theme.accent
        } else {
            theme.border_strong
        })
        .cursor_pointer()
        .when(on, |element| element.justify_end())
        .child(
            div()
                .flex_none()
                .size(px(16.0))
                .rounded(px(radius::FULL))
                .bg(if on { theme.on_accent } else { theme.surface }),
        )
}

pub fn empty_state(theme: &Theme, title: &str, body: &str, icon_path: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .p_6()
        .text_center()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(56.0))
                .rounded(px(radius::XL))
                .bg(theme.accent_soft)
                .child(
                    svg()
                        .path(icon_path)
                        .size(px(24.0))
                        .text_color(theme.text_faint),
                ),
        )
        .child(
            div()
                .text_size(px(text_size::XL))
                .line_height(px(30.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title.to_string()),
        )
        .child(
            div()
                .max_w(px(480.0))
                .text_size(px(text_size::BASE))
                .line_height(px(23.0))
                .text_color(theme.text_muted)
                .child(body.to_string()),
        )
}
