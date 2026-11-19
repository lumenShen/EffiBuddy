//! Mobile composer: one floating glass capsule with `+`, the shared text
//! input and a circular send/stop key, plus the queue strip above it.
//!
//! The input itself is the desktop's `composer` entity (same drafts, same `/`
//! and `@` menus, same submission path), so the phone cannot drift from the
//! desktop's input behaviour.

use effibuddy_ui::{Theme, icons, mobile as geo, radius, space};
use gpui::{AnyElement, Context, IntoElement, SharedString, Window, div, prelude::*, px, svg};

use crate::app::AppView;
use crate::mobile::glass;

impl AppView {
    pub(crate) fn render_mobile_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let running = self.active_run.is_some();
        let empty = self.composer_state.read(cx).content().trim().is_empty()
            && !self.composer_has_attachments();

        let mut column = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::SM))
            .px(px(geo::GUTTER))
            .pb(px(space::LG + self.mobile.insets.bottom))
            .pt(px(space::SM));

        // Subagent pill: the phone has no side pane, so a running child would
        // otherwise be invisible. One tap raises the roster sheet.
        let running_agents = self
            .subagent_tasks
            .iter()
            .filter(|task| task.status == effibuddy_agent_core::SubagentStatus::Running)
            .count();
        if running_agents > 0 {
            column = column.child(
                div()
                    .id("mobile-agents-pill")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(theme.glass)
                    .border_1()
                    .border_color(theme.glass_border)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_alt))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_mobile_sheet(crate::mobile::MobileSheet::Agents, cx);
                    }))
                    .child(
                        svg()
                            .path(icons::SPARKLES)
                            .size(px(12.0))
                            .text_color(theme.accent),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format!("{running_agents} 个子代理运行中")),
                    )
                    .child(
                        svg()
                            .path(icons::CHEVRON_RIGHT)
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                    ),
            );
        }

        // Queue / steer strip: the reference shows the queued text with
        // 「立即发送 / 改为插话 / 撤回」 right under the transcript.
        for row in self.mobile_queue_rows() {
            column = column.child(render_queue_row(&theme, row, cx));
        }

        let send = if running {
            div()
                .id("mobile-stop")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(32.0))
                .rounded(px(geo::RADIUS_CHIP))
                .bg(theme.danger_soft)
                .cursor_pointer()
                .hover(|style| style.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .child(
                    svg()
                        .path(icons::STOP)
                        .size(px(12.0))
                        .text_color(theme.danger),
                )
        } else if empty {
            div()
                .id("mobile-send-idle")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(32.0))
                .rounded(px(geo::RADIUS_CHIP))
                .bg(theme.disabled_bg)
                .child(
                    svg()
                        .path(icons::ARROW_UP)
                        .size(px(15.0))
                        .text_color(theme.disabled_fg),
                )
        } else {
            div()
                .id("mobile-send")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(32.0))
                .rounded(px(geo::RADIUS_CHIP))
                .bg(theme.accent)
                .cursor_pointer()
                .hover(|style| style.bg(theme.accent_hover))
                .on_click(cx.listener(|this, _, window, cx| {
                    let text = this.composer_state.read(cx).content().to_string();
                    this.composer_state
                        .update(cx, |state, cx| state.clear(window, cx));
                    this.send_message(text, false, window, cx);
                }))
                .child(
                    svg()
                        .path(icons::ARROW_UP)
                        .size(px(15.0))
                        .text_color(theme.on_accent),
                )
        };

        let add = div()
            .id("mobile-composer-add")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(geo::RADIUS_CHIP))
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(cx.listener(|this, _, _, cx| this.pick_composer_files(cx)))
            .child(
                svg()
                    .path(icons::PLUS)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );

        // The input owns its own placeholder (shared with the desktop draft);
        // the phone only swaps it while a run is live so the same line reads
        // 「生成中 · 可直接插话」 like the reference.
        let _ = if running {
            "生成中 · 可直接插话"
        } else if empty {
            "给 EffiBuddy 发消息…"
        } else {
            ""
        };

        let focused = self
            .composer_state
            .read(cx)
            .focus_handle()
            .is_focused(window);

        let input_row = div()
            .flex()
            .items_end()
            .gap(px(space::SM))
            .child(add)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.0))
                    .py(px(7.0))
                    .child(self.composer.clone()),
            )
            .child(send);

        let capsule = glass(&theme)
            .w_full()
            .rounded(px(26.0))
            .border_color(if focused {
                theme.focus_ring
            } else {
                theme.glass_border
            })
            .px(px(space::SM))
            .py(px(space::XS))
            .on_mouse_down(gpui::MouseButton::Left, {
                let state = self.composer_state.clone();
                move |_, window, cx| {
                    let _ = state.update(cx, |state, _| {
                        window.focus(&state.focus_handle());
                    });
                }
            });

        column = column.child(capsule.child(input_row));

        // Secondary line: permission mode + model, both one tap away.
        column = column.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(space::XS))
                .child(self.mobile_permission_chip(&theme, cx))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(self.mobile_model_chip(&theme, cx))
                        .when(running, |element| {
                            element.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child("Esc 停止"),
                            )
                        }),
                ),
        );

        column.into_any_element()
    }

    fn mobile_permission_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.permission_mode();
        let (label, tone, bg) = match mode {
            effibuddy_agent_core::tool::PermissionMode::Default => {
                ("默认询问", theme.warning, theme.warning_soft)
            }
            effibuddy_agent_core::tool::PermissionMode::AcceptEdits => {
                ("自动编辑", theme.accent, theme.accent_soft)
            }
            effibuddy_agent_core::tool::PermissionMode::Plan => {
                ("计划模式", theme.warning, theme.warning_soft)
            }
            effibuddy_agent_core::tool::PermissionMode::FullAccess => {
                ("完全访问", theme.success, theme.success_soft)
            }
        };
        div()
            .id("mobile-permission")
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(24.0))
            .px(px(9.0))
            .rounded(px(geo::RADIUS_CHIP))
            .bg(bg)
            .cursor_pointer()
            .hover(|style| style.opacity(0.85))
            .on_click(cx.listener(|this, _, _, cx| this.cycle_permission_mode(cx)))
            .child(
                svg()
                    .path(icons::WRENCH)
                    .size(px(10.0))
                    .text_color(tone),
            )
            .child(div().text_size(px(10.5)).text_color(tone).child(label))
            .into_any_element()
    }

    fn mobile_model_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let resolved = self.active_resolved_model();
        div()
            .id("mobile-composer-model")
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(24.0))
            .px(px(9.0))
            .rounded(px(geo::RADIUS_CHIP))
            .bg(theme.surface_alt)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.open_mobile_sheet(crate::mobile::MobileSheet::Model, cx);
            }))
            .child(
                div()
                    .size(px(5.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(if resolved.live {
                        theme.success
                    } else {
                        theme.warning
                    }),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(resolved.model_label().to_string()),
            )
            .into_any_element()
    }
}

/// One queued message: chip + preview + the three actions the reference
/// offers while a turn is running.
fn render_queue_row(
    theme: &Theme,
    row: crate::app::MobileQueueRow,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let (chip, tone, bg) = if row.steer {
        ("待注入", theme.accent, theme.accent_soft)
    } else {
        ("排队中", theme.text_muted, theme.surface_alt)
    };
    let send_id = row.id.clone();
    let drop_id = row.id.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .rounded(px(radius::MD))
        .bg(theme.glass)
        .border_1()
        .border_color(theme.glass_border)
        .px(px(10.0))
        .py(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(18.0))
                        .px(px(6.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .bg(bg)
                        .text_size(px(10.0))
                        .text_color(tone)
                        .child(chip),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(row.text),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("mobile-queue-drop-{drop_id}")))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_pending_input(&drop_id, cx);
                        }))
                        .child(
                            svg()
                                .path(icons::X)
                                .size(px(11.0))
                                .text_color(theme.text_faint),
                        ),
                ),
        )
        .child(div().flex().justify_end().child(
            div()
                .id(SharedString::from(format!("mobile-queue-send-{send_id}")))
                .h(px(22.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .rounded(px(geo::RADIUS_CHIP))
                .bg(theme.surface_alt)
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send_pending_input_now(&send_id, cx);
                }))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child("立即发送"),
                ),
        ))
        .into_any_element()
}
