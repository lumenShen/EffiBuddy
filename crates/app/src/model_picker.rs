//! Composer model picker.

use effibuddy_agent_core::lock_session;
use effibuddy_ui::{
    Button, ButtonSize, ButtonVariant, IconButton, ease_out_cubic, icons, overlay_motion_duration,
    shadow_float,
};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, IntoElement, MouseButton, Pixels, Point,
    SharedString, Window, anchored, div, point, prelude::*, px, svg,
};

use crate::app::AppView;

#[derive(Default)]
pub(crate) struct ModelMenuState {
    pub open: bool,
    pub position: Option<Point<Pixels>>,
    /// Provider currently browsed in the popover. `None` falls back to the
    /// active session's provider.
    pub provider_id: Option<String>,
}

impl AppView {
    pub(crate) fn open_model_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let active_provider = self
            .active_shared()
            .map(|shared| {
                let session = lock_session(&shared);
                self.models.resolve_session(&session).provider.id
            })
            .or_else(|| {
                self.models
                    .default_selection()
                    .map(|selection| selection.provider_id)
            });
        self.model_menu.provider_id = active_provider;
        self.model_menu.position = Some(position);
        self.model_menu.open = true;
        cx.notify();
    }

    pub(crate) fn close_model_menu(&mut self, cx: &mut Context<Self>) {
        self.model_menu.open = false;
        cx.notify();
    }

    pub(crate) fn apply_model_selection(
        &mut self,
        provider_id: &str,
        model_id: &str,
        reasoning_level: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = &self.active else {
            self.notice = Some("没有活动会话，无法切换模型。".to_string());
            cx.notify();
            return;
        };
        {
            let mut session = lock_session(&active.shared);
            session.set_model_selection(provider_id, model_id, reasoning_level);
        }
        self.persist_active(cx);
        self.model_menu.open = false;
        let running = self.active_run.is_some();
        self.notice = Some(if running {
            "模型已切换，下一轮生效。".to_string()
        } else {
            "模型已切换。".to_string()
        });
        cx.notify();
    }

    fn set_reasoning_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        let Some(active) = &self.active else {
            return;
        };
        {
            let mut session = lock_session(&active.shared);
            session.reasoning_level = level.clone();
            session.touch();
        }
        self.persist_active(cx);
        self.notice = Some(match level {
            Some(level) => format!("推理档位已切换为 {level}。"),
            None => "已关闭推理档位选择。".to_string(),
        });
        cx.notify();
    }

    pub(crate) fn render_model_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.model_menu.open {
            return None;
        }
        let theme = self.theme.clone();
        let position = self.model_menu.position?;
        let active = self.active_shared().map(|shared| {
            let session = lock_session(&shared);
            self.models.resolve_session(&session)
        });
        let selected_provider = self
            .model_menu
            .provider_id
            .clone()
            .or_else(|| active.as_ref().map(|resolved| resolved.provider.id.clone()))
            .or_else(|| {
                self.models
                    .default_selection()
                    .map(|selection| selection.provider_id)
            });
        let providers: Vec<_> = self
            .models
            .file()
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .cloned()
            .collect();
        let provider = selected_provider
            .as_deref()
            .and_then(|id| providers.iter().find(|provider| provider.id == id))
            .or_else(|| providers.first());
        let models: Vec<_> = provider
            .map(|provider| provider.enabled_models().cloned().collect())
            .unwrap_or_default();

        let provider_chips: Vec<AnyElement> = providers
            .iter()
            .map(|candidate| {
                let active_chip = provider
                    .map(|provider| provider.id == candidate.id)
                    .unwrap_or(false);
                let provider_id = candidate.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "model-menu-provider-{}",
                        candidate.id
                    )))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.0))
                    .h(px(24.0))
                    .px(px(8.0))
                    .rounded(px(999.0))
                    .cursor_pointer()
                    .when(active_chip, |element| element.bg(theme.accent_soft))
                    .when(!active_chip, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model_menu.provider_id = Some(provider_id.clone());
                        cx.notify();
                    }))
                    .child(div().size(px(5.0)).rounded(px(999.0)).bg(
                        if candidate.has_credentials() {
                            theme.success
                        } else {
                            theme.warning
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(if active_chip {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(candidate.display_name().to_string()),
                    )
                    .into_any_element()
            })
            .collect();

        let model_rows: Vec<AnyElement> = models
            .iter()
            .map(|model| {
                let current = active.as_ref().is_some_and(|resolved| {
                    resolved.provider.id == provider.map(|p| p.id.as_str()).unwrap_or_default()
                        && resolved.model.id == model.id
                });
                let provider_id = provider
                    .map(|provider| provider.id.clone())
                    .unwrap_or_default();
                let model_id = model.id.clone();
                let reasoning = model.default_reasoning_level.clone();
                div()
                    .id(SharedString::from(format!(
                        "model-menu-model-{}-{}",
                        provider_id, model.id
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_h(px(34.0))
                    .px_2()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(current, |element| element.bg(theme.accent_soft))
                    .when(!current, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let reasoning = this
                            .models
                            .file()
                            .provider(&provider_id)
                            .and_then(|provider| provider.model(&model_id))
                            .and_then(|model| model.default_reasoning_level.clone())
                            .or(reasoning.clone());
                        this.apply_model_selection(&provider_id, &model_id, reasoning, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(model.display_name().to_string()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(theme.mono_font())
                                    .text_size(px(9.5))
                                    .text_color(theme.text_faint)
                                    .child(model.id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(format_context(model.context_window)),
                    )
                    .when(current, |element| {
                        element.child(
                            svg()
                                .path(icons::CHECK)
                                .size(px(12.0))
                                .text_color(theme.success),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        let current_model = active.as_ref().map(|resolved| resolved.model.clone());
        let reasoning_levels: Vec<_> = current_model
            .as_ref()
            .map(|model| model.reasoning_levels.clone())
            .unwrap_or_default();
        let selected_level = active
            .as_ref()
            .and_then(|resolved| resolved.reasoning_level.clone());
        let reasoning_chips: Vec<AnyElement> = reasoning_levels
            .iter()
            .map(|level| {
                let active_level = selected_level.as_deref() == Some(level.value.as_str());
                let value = level.value.clone();
                Button::new(
                    SharedString::from(format!("reasoning-{}", level.value)),
                    level.display_label().to_string(),
                    &theme,
                )
                .variant(if active_level {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                })
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_reasoning_level(Some(value.clone()), cx);
                }))
                .into_any_element()
            })
            .collect();

        Some(
            div()
                .id("model-menu-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_model_menu(cx);
                    }),
                )
                .child(
                    anchored()
                        .position(position)
                        .offset(point(px(-350.0), px(-420.0)))
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id("model-menu")
                                .w(px(360.0))
                                .max_h(px(480.0))
                                .rounded(px(18.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(shadow_float(&theme))
                                .p_2()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .px_1()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_size(px(11.5))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child("选择模型"),
                                        )
                                        .child(
                                            IconButton::new("model-menu-close", icons::X, &theme)
                                                .size(22.0, 11.0)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_model_menu(cx);
                                                })),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("model-menu-providers")
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .overflow_x_scroll()
                                        .pb_1()
                                        .children(provider_chips),
                                )
                                .child(
                                    div()
                                        .id("model-menu-models")
                                        .max_h(px(280.0))
                                        .overflow_y_scroll()
                                        .flex()
                                        .flex_col()
                                        .gap(px(1.0))
                                        .when(model_rows.is_empty(), |element| {
                                            element.child(
                                                div()
                                                    .px_2()
                                                    .py_3()
                                                    .text_size(px(11.0))
                                                    .text_color(theme.text_faint)
                                                    .child("该供应商还没有可用模型。"),
                                            )
                                        })
                                        .children(model_rows),
                                )
                                .when(!reasoning_chips.is_empty(), |element| {
                                    element
                                        .child(div().h(px(1.0)).bg(theme.border).my_1())
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .gap_2()
                                                .px(px(4.0))
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .text_color(theme.text_faint)
                                                        .child("推理档位"),
                                                )
                                                .children(reasoning_chips)
                                                .child(
                                                    Button::new("reasoning-auto", "自动", &theme)
                                                        .variant(if selected_level.is_none() {
                                                            ButtonVariant::Primary
                                                        } else {
                                                            ButtonVariant::Ghost
                                                        })
                                                        .size(ButtonSize::Sm)
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.set_reasoning_level(None, cx);
                                                        })),
                                                ),
                                        )
                                })
                                .child(div().h(px(1.0)).bg(theme.border).my_1())
                                .child(
                                    Button::new("model-menu-manage", "管理模型…", &theme)
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .icon(icons::SETTINGS)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.model_menu.open = false;
                                            this.open_settings_models(window, cx);
                                        })),
                                )
                                .with_animation(
                                    "model-menu-enter",
                                    Animation::new(overlay_motion_duration())
                                        .with_easing(ease_out_cubic),
                                    |element, delta| {
                                        element.opacity(delta).mt(px(4.0 * (1.0 - delta)))
                                    },
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

fn format_context(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m ctx", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k ctx", tokens / 1_000)
    } else {
        format!("{tokens} ctx")
    }
}
