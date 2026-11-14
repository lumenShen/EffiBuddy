//! Small building blocks: spinner, badge, avatar, card, divider, keycap.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, AnyView, App, BoxShadow, ClickEvent, Div, Element,
    ElementId, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    Pixels, Render, RenderOnce, SharedString, Transformation, Window, div, percentage, prelude::*,
    px, radians, svg,
};

use crate::assets::icons;
use crate::motion::{CONTROL_MOTION, SWITCH_MOTION, ease_out_cubic, mix_color, reduce_motion};
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

/// A 36×20 switch track with a sliding knob. Attach an `on_click` to the
/// returned element; the whole track is the hit target.
pub fn switch(theme: &Theme, on: bool) -> Switch {
    Switch {
        id: SharedString::from("switch"),
        theme: theme.clone(),
        on,
        on_click: None,
    }
}

struct SwitchState {
    start: Instant,
    from: f32,
    to: f32,
}

impl SwitchState {
    fn value_at(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.start).as_secs_f32()
            / SWITCH_MOTION.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * ease_out_cubic(t)
    }
}

/// An animated toggle. The track color crossfades, and the knob travels 14px
/// (the same 3px border+padding inset the old flex layout used). A stable id
/// is required for the animation state to survive re-renders.
pub struct Switch {
    id: SharedString,
    theme: Theme,
    on: bool,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl Switch {
    pub fn id(mut self, id: impl Into<SharedString>) -> Self {
        self.id = id.into();
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    fn track(&self, value: f32) -> AnyElement {
        let target = if self.on { 1.0 } else { 0.0 };
        let value = if reduce_motion() { target } else { value };
        let track_bg = mix_color(self.theme.disabled_bg, self.theme.accent, value);
        let track_border = mix_color(self.theme.border_strong, self.theme.accent, value);
        let knob = mix_color(self.theme.surface, self.theme.on_accent, value);
        let inner_id = ElementId::Name(SharedString::from(format!("{}-hit", self.id)));

        let mut track = div()
            .id(inner_id)
            .relative()
            .flex_none()
            .w(px(36.0))
            .h(px(20.0))
            .rounded(px(radius::FULL))
            .bg(track_bg)
            .border_1()
            .border_color(track_border)
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .top(px(2.0))
                    .left(px(3.0 + 14.0 * value))
                    .size(px(16.0))
                    .rounded(px(radius::FULL))
                    .bg(knob),
            );

        if let Some(handler) = self.on_click.clone() {
            track = track.on_click(move |event, window, cx| handler(event, window, cx));
        }
        track.into_any_element()
    }
}

impl IntoElement for Switch {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Switch {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(self.id.clone()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let target = if self.on { 1.0 } else { 0.0 };
        let now = Instant::now();
        let reduce = reduce_motion();
        let (value, animating) = window.with_element_state(
            global_id.expect("switch requires an element id"),
            |state, _window| {
                let mut state = state.unwrap_or_else(|| SwitchState {
                    // A fresh switch is already settled, not entering.
                    start: now - SWITCH_MOTION,
                    from: target,
                    to: target,
                });
                if state.to != target {
                    if reduce {
                        state.from = target;
                        state.to = target;
                        state.start = now - SWITCH_MOTION;
                    } else {
                        state.from = state.value_at(now);
                        state.to = target;
                        state.start = now;
                    }
                }
                let t = (now.saturating_duration_since(state.start).as_secs_f32()
                    / SWITCH_MOTION.as_secs_f32())
                .clamp(0.0, 1.0);
                let value = if reduce {
                    target
                } else {
                    state.from + (state.to - state.from) * ease_out_cubic(t)
                };
                ((value, !reduce && state.from != state.to && t < 1.0), state)
            },
        );
        if animating {
            window.request_animation_frame();
        }

        let mut track = self.track(value);
        let layout_id = track.request_layout(window, cx);
        (layout_id, track)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        track: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        track.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        track: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        track.paint(window, cx);
    }
}

/// Disclosure chevron: one right-pointing SVG rotated 90° when expanded.
pub fn disclosure_chevron(
    id: impl Into<SharedString>,
    expanded: bool,
    color: Hsla,
    size: f32,
) -> DisclosureChevron {
    DisclosureChevron {
        id: id.into(),
        expanded,
        color,
        size,
    }
}

struct ChevronState {
    start: Instant,
    from: f32,
    to: f32,
}

impl ChevronState {
    fn value_at(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.start).as_secs_f32()
            / CONTROL_MOTION.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * ease_out_cubic(t)
    }
}

pub struct DisclosureChevron {
    id: SharedString,
    expanded: bool,
    color: Hsla,
    size: f32,
}

impl IntoElement for DisclosureChevron {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for DisclosureChevron {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(self.id.clone()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let target = if self.expanded { 1.0 } else { 0.0 };
        let now = Instant::now();
        let reduce = reduce_motion();
        let (value, animating) = window.with_element_state(
            global_id.expect("disclosure chevron requires an element id"),
            |state, _window| {
                let mut state = state.unwrap_or_else(|| ChevronState {
                    start: now - CONTROL_MOTION,
                    from: target,
                    to: target,
                });
                if state.to != target {
                    if reduce {
                        state.from = target;
                        state.to = target;
                        state.start = now - CONTROL_MOTION;
                    } else {
                        state.from = state.value_at(now);
                        state.to = target;
                        state.start = now;
                    }
                }
                let t = (now.saturating_duration_since(state.start).as_secs_f32()
                    / CONTROL_MOTION.as_secs_f32())
                .clamp(0.0, 1.0);
                let value = if reduce {
                    target
                } else {
                    state.from + (state.to - state.from) * ease_out_cubic(t)
                };
                ((value, !reduce && state.from != state.to && t < 1.0), state)
            },
        );
        if animating {
            window.request_animation_frame();
        }

        let rotation = radians(std::f32::consts::FRAC_PI_2 * value);
        let mut icon = svg()
            .path(icons::CHEVRON_RIGHT)
            .size(px(self.size))
            .text_color(self.color)
            .flex_none()
            .with_transformation(Transformation::rotate(rotation))
            .into_any_element();
        let layout_id = icon.request_layout(window, cx);
        (layout_id, icon)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        icon: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        icon.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        icon: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        icon.paint(window, cx);
    }
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
