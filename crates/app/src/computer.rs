//! Computer-use UI: the floating control bar and the composer hint.
//!
//! The runtime lives in `effibuddy-computer`. Permission is *not* decided here:
//! `computer` is a regular tool (`ToolClass::Device`) and the session's
//! `PermissionMode` approves or denies it, for the main agent and for subagents
//! alike. This module only renders the takeover/stop bar and the composer chip.

use std::sync::Arc;

use effibuddy_agent_core::tool::PermissionMode;
use effibuddy_computer::{ComputerRuntime, ComputerUseMode, OverlayEvent, OverlayEventKind};
use effibuddy_ui::{Button, ButtonSize, ButtonVariant, Spinner, Theme, dot, shadow_float};
use gpui::prelude::*;
use gpui::{AnyElement, Context, IntoElement, Render, SharedString, WeakEntity, Window, div, px};

use crate::app::AppView;

pub(crate) struct ComputerOverlayView {
    runtime: Arc<ComputerRuntime>,
    app: WeakEntity<AppView>,
    theme: Theme,
    session_id: Option<String>,
    message: String,
    paused: bool,
}

impl ComputerOverlayView {
    pub fn new(runtime: Arc<ComputerRuntime>, app: WeakEntity<AppView>, theme: Theme) -> Self {
        Self {
            runtime,
            app,
            theme,
            session_id: None,
            message: "正在连接桌面…".to_string(),
            paused: false,
        }
    }

    pub fn apply(&mut self, event: &OverlayEvent, cx: &mut Context<Self>) {
        self.session_id = Some(event.session_id.clone());
        match &event.kind {
            OverlayEventKind::Show { message, paused }
            | OverlayEventKind::Update { message, paused } => {
                self.message = message.clone();
                self.paused = *paused;
            }
            OverlayEventKind::Hide | OverlayEventKind::CaptureHide => {}
        }
        cx.notify();
    }

    fn take_over(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.take_over(session_id);
            self.paused = true;
            self.message =
                "你已接管操作。完成后点「我帮你点好了」继续，或点「停止操作」。".to_string();
            cx.notify();
        }
    }

    fn resume(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.resume(session_id);
            self.paused = false;
            self.message = "EffiBuddy 正在继续操作…".to_string();
            cx.notify();
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.stop(session_id);
        }
        // Stop the current run immediately instead of waiting for the model's
        // next tool call to notice the stopped flag.
        let _ = self.app.update(cx, |app, cx| app.interrupt(cx));
    }
}

impl Render for ComputerOverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let status_color = if self.paused {
            theme.warning
        } else {
            theme.accent
        };
        let status_icon: AnyElement = if self.paused {
            dot(&theme, status_color, 8.0).into_any_element()
        } else {
            Spinner::new(status_color, 14.0).into_any_element()
        };
        let primary_label: SharedString = if self.paused {
            "我帮你点好了".into()
        } else {
            "接管操作".into()
        };

        div()
            .size_full()
            .px(px(16.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .rounded(px(18.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow(shadow_float(&theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .min_w(px(0.0))
                    .child(status_icon)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("EffiBuddy 正在帮您使用电脑"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(self.message.clone()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        Button::new("computer-takeover", primary_label, &theme)
                            .size(ButtonSize::Sm)
                            .variant(if self.paused {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Secondary
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.paused {
                                    this.resume(cx);
                                } else {
                                    this.take_over(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("computer-stop", "停止操作", &theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Danger)
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    ),
            )
    }
}

/// Compact chip text for the composer. The chip only advertises the capability;
/// whether a call interrupts the user is the permission mode's business, so the
/// tooltip points at the mode chip instead of promising a consent card.
pub(crate) fn composer_hint(
    mode: ComputerUseMode,
    vision: bool,
    permission_mode: PermissionMode,
) -> Option<(&'static str, &'static str)> {
    if !mode.is_enabled() {
        return None;
    }
    let label = if permission_mode == PermissionMode::FullAccess {
        "电脑操作已允许 · 完全访问"
    } else {
        "电脑操作已开启 · 按键鼠审批"
    };
    let tooltip = if vision {
        "已允许 AI 使用你的电脑键鼠模拟输入；是否弹审批卡由当前权限模式决定。点击打开设置。"
    } else {
        "已允许 AI 使用键鼠模拟输入，但当前模型未开启视觉能力，截图无法发给模型；点击打开设置。"
    };
    Some((label, tooltip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_follow_the_capability_switch_and_permission_mode() {
        assert!(composer_hint(ComputerUseMode::Off, true, PermissionMode::Default).is_none());
        assert_eq!(
            composer_hint(ComputerUseMode::On, true, PermissionMode::Default)
                .unwrap()
                .0,
            "电脑操作已开启 · 按键鼠审批"
        );
        assert_eq!(
            composer_hint(ComputerUseMode::On, true, PermissionMode::FullAccess)
                .unwrap()
                .0,
            "电脑操作已允许 · 完全访问"
        );
        // The vision warning is carried by the tooltip, not by the label.
        assert!(
            composer_hint(ComputerUseMode::On, false, PermissionMode::Default)
                .unwrap()
                .1
                .contains("视觉")
        );
    }
}
