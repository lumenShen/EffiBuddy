//! Inter-agent message card for the shared chat stream.
//!
//! The main transcript and every read-only subagent transcript use the same
//! `ChatList`, so this renderer gives agent-to-agent traffic a stable identity
//! without inventing a second message UI.

use effibuddy_agent_core::AgentMessageMeta;
use effibuddy_ui::{BadgeTone, Theme, badge, icons};
use gpui::{AnyElement, IntoElement, div, prelude::*, px, svg};

pub(crate) fn render_agent_message(
    theme: &Theme,
    meta: &AgentMessageMeta,
    text: &str,
) -> AnyElement {
    let soft = {
        let mut color = theme.agent_blue;
        color.a = if theme.is_dark() { 0.20 } else { 0.10 };
        color
    };
    let mut card = div()
        .w_full()
        .max_w(px(660.0))
        .rounded(px(14.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(22.0))
                        .rounded(px(6.0))
                        .bg(soft)
                        .child(
                            svg()
                                .path(icons::BRANCH)
                                .size(px(11.0))
                                .text_color(theme.agent_blue),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(meta.from_name.clone()),
                        )
                        .child(
                            svg()
                                .path(icons::CHEVRON_RIGHT)
                                .size(px(9.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(meta.to_name.clone()),
                        ),
                )
                .child(badge(theme, meta.kind.label(), BadgeTone::Neutral))
                .child(div().flex_1())
                .child(
                    div()
                        .font_family(theme.mono_font())
                        .text_size(px(9.0))
                        .text_color(theme.text_faint)
                        .child(meta.from_id.clone()),
                ),
        );
    if !text.trim().is_empty() {
        card = card.child(
            div()
                .text_size(px(12.0))
                .line_height(px(19.0))
                .text_color(theme.text_muted)
                .child(text.trim().to_string()),
        );
    }
    if let Some(data) = &meta.data {
        let json = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
        card = card.child(
            div()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.code_bg)
                .px(px(9.0))
                .py(px(7.0))
                .font_family(theme.mono_font())
                .text_size(px(10.0))
                .line_height(px(15.0))
                .text_color(theme.text_muted)
                .child(truncate(&json, 2400)),
        );
    }
    div()
        .w_full()
        .flex()
        .justify_start()
        .child(card)
        .into_any_element()
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}
