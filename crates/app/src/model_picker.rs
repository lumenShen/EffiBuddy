//! Composer model picker.
//!
//! 主菜单纵向列出已配置好凭据的供应商，悬停后在右侧弹出 child-menu 选择该
//! 供应商的模型；推理档位区固定在菜单底部，作用于会话当前模型。

use effibuddy_agent_core::{ProviderConfig, ResolvedModel, lock_session};
use effibuddy_ui::{
    Button, ButtonSize, ButtonVariant, IconButton, Theme, ease_out_cubic, icons,
    overlay_motion_duration, shadow_float,
};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, IntoElement, MouseButton, Pixels, Point,
    SharedString, Window, anchored, div, point, prelude::*, px, relative, svg,
};

use crate::app::AppView;

/// 主菜单宽度。
const MENU_WIDTH: f32 = 232.0;
/// 模型子菜单宽度。
const SUBMENU_WIDTH: f32 = 264.0;
/// 子菜单最大高度；模型更多时在子菜单内滚动。
const SUBMENU_MAX_H: f32 = 300.0;
/// 供应商行高度。
const MENU_ROW_H: f32 = 30.0;
/// 供应商行占位高度（含间距），用于展开方向估算。
const MENU_ROW_PITCH: f32 = MENU_ROW_H + 2.0;
/// 菜单顶部到第一行供应商行顶部的估算距离（内边距 + 标题 + 间距）。
const MENU_HEAD_H: f32 = 44.0;
/// 菜单固定部分（内边距、标题、分隔线、管理按钮等）的估算高度。
const MENU_FIXED_H: f32 = 128.0;
/// 推理档位区（分隔线 + 一行档位按钮）的估算高度。
const MENU_REASONING_H: f32 = 40.0;

#[derive(Default)]
pub(crate) struct ModelMenuState {
    pub open: bool,
    pub position: Option<Point<Pixels>>,
    /// 子菜单展开中的供应商。
    pub provider_id: Option<String>,
}

impl AppView {
    pub(crate) fn open_model_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        // The composer's `/`/`@` panel and the model popover are both anchored
        // to the composer; never show them at once.
        self.composer_menu.close();
        // 打开时默认展开会话正在使用的供应商（未配置则落到首个已配置供应商）。
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
        let Some(shared) = self.active_shared() else {
            self.notice = Some("没有活动会话，无法切换模型。".to_string());
            cx.notify();
            return;
        };
        {
            let mut session = lock_session(&shared);
            session.set_model_selection(provider_id, model_id, reasoning_level);
        }
        // Model configuration must survive switching away or closing the app
        // immediately, so it cannot wait for the throttled transcript writer.
        if let Err(error) = self.save_active_metadata(&shared) {
            self.last_error = Some(error);
        }
        self.model_menu.open = false;
        let running = self.active_run.is_some();
        self.notice = Some(if running {
            "模型已切换，下一轮生效。".to_string()
        } else {
            "模型已切换。".to_string()
        });
        cx.notify();
    }

    /// 从子菜单选择模型。推理档位为自动时保持自动——不回填模型默认档位，
    /// 请求也就不携带 reasoning 字段；会话已明确选择的档位若新模型仍支持则保留。
    fn select_menu_model(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        let current_level = self
            .active_shared()
            .map(|shared| lock_session(&shared).reasoning_level.clone())
            .unwrap_or_default();
        let keeps_level = current_level.as_deref().is_some_and(|level| {
            self.models
                .file()
                .provider(provider_id)
                .and_then(|provider| provider.model(model_id))
                .is_some_and(|model| model.supports_reasoning_level(level))
        });
        let reasoning = if keeps_level { current_level } else { None };
        self.apply_model_selection(provider_id, model_id, reasoning, cx);
    }

    fn set_reasoning_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = lock_session(&shared);
            session.reasoning_level = level.clone();
            session.touch();
        }
        if let Err(error) = self.save_active_metadata(&shared) {
            self.last_error = Some(error);
        }
        self.notice = Some(match level {
            Some(level) => format!("推理档位已切换为 {level}。"),
            None => "推理档位已设为自动，请求不再携带档位字段。".to_string(),
        });
        cx.notify();
    }

    pub(crate) fn render_model_menu(
        &self,
        window: &mut Window,
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
        let active_provider_id = active.as_ref().map(|resolved| resolved.provider.id.clone());

        // 只列出已经填写好凭据的供应商；没配 Key 的在设置页补全后再来这里。
        let providers: Vec<_> = self
            .models
            .file()
            .providers
            .iter()
            .filter(|provider| provider.enabled && provider.has_credentials())
            .cloned()
            .collect();

        // 子菜单展开中的供应商；打开时落在会话正在使用的那个上。
        let expanded_provider = self
            .model_menu
            .provider_id
            .clone()
            .filter(|id| providers.iter().any(|provider| &provider.id == id))
            .or_else(|| {
                active_provider_id
                    .clone()
                    .filter(|id| providers.iter().any(|provider| &provider.id == id))
                    .or_else(|| providers.first().map(|provider| provider.id.clone()))
            });

        let reasoning_levels: Vec<_> = active
            .as_ref()
            .map(|resolved| resolved.model.reasoning_levels.clone())
            .unwrap_or_default();

        // 菜单以左下角锚在按钮上方 8px；行上/下方剩余空间决定子菜单向上还是
        // 向下展开。供应商行数少时菜单矮、行下方空间不足，因此默认向上。
        let viewport = window.viewport_size();
        let menu_h = px(MENU_FIXED_H
            + providers.len() as f32 * MENU_ROW_PITCH
            + if reasoning_levels.is_empty() {
                0.0
            } else {
                MENU_REASONING_H
            });
        let menu_bottom_y = (position.y - px(8.0)).min(viewport.height - px(12.0));
        let menu_top_y = menu_bottom_y - menu_h;
        let first_row_top_y = menu_top_y + px(MENU_HEAD_H);
        // 主菜单右缘（可能被 snap_to_window 收进窗口内）。
        let menu_right_x = (position.x - px(8.0)).min(viewport.width - px(12.0));
        let opens_right = menu_right_x + px(6.0) + px(SUBMENU_WIDTH) <= viewport.width - px(12.0);

        let provider_rows: Vec<AnyElement> = providers
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let expanded = expanded_provider.as_deref() == Some(candidate.id.as_str());
                let is_current_provider =
                    active_provider_id.as_deref() == Some(candidate.id.as_str());
                let provider_id = candidate.id.clone();
                let hover_provider_id = provider_id.clone();
                let click_provider_id = provider_id.clone();

                let mut row = div()
                    .id(SharedString::from(format!(
                        "model-menu-provider-{}",
                        candidate.id
                    )))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(MENU_ROW_H))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(expanded, |element| element.bg(theme.accent_soft))
                    .when(!expanded, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    // 悬停即切换子菜单；移出不清状态，避免与子菜单之间的间隙
                    // 导致闪烁，也与常见菜单“最后悬停项保持展开”一致。
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered
                            && this.model_menu.provider_id.as_deref()
                                != Some(hover_provider_id.as_str())
                        {
                            this.model_menu.provider_id = Some(hover_provider_id.clone());
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.model_menu.provider_id.as_deref()
                            != Some(click_provider_id.as_str())
                        {
                            this.model_menu.provider_id = Some(click_provider_id.clone());
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(candidate.display_name().to_string()),
                    )
                    .when(is_current_provider, |element| {
                        element.child(
                            svg()
                                .path(icons::CHECK)
                                .size(px(12.0))
                                .text_color(theme.success),
                        )
                    })
                    .child(
                        svg()
                            .path(icons::CHEVRON_RIGHT)
                            .size(px(10.0))
                            .text_color(theme.text_faint),
                    );
                if expanded {
                    let room_below_row = viewport.height
                        - px(12.0)
                        - (first_row_top_y + px(index as f32 * MENU_ROW_PITCH));
                    let room_above_row =
                        first_row_top_y + px(index as f32 * MENU_ROW_PITCH + MENU_ROW_H);
                    row = row.child(self.render_provider_submenu(
                        candidate,
                        active.as_ref(),
                        room_below_row,
                        room_above_row,
                        opens_right,
                        &theme,
                        cx,
                    ));
                }
                row.into_any_element()
            })
            .collect();

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
                        .anchor(gpui::Corner::BottomLeft)
                        .offset(point(px(-(MENU_WIDTH + 8.0)), px(-8.0)))
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id("model-menu")
                                .w(px(MENU_WIDTH))
                                .rounded(px(14.0))
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
                                .when(providers.is_empty(), |element| {
                                    element.child(
                                        div()
                                            .px_2()
                                            .py_3()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_faint)
                                            .child(
                                                "还没有已配置的供应商，请先在设置 → 模型中填写。",
                                            ),
                                    )
                                })
                                .when(!providers.is_empty(), |element| {
                                    element.child(
                                        div()
                                            .id("model-menu-providers")
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .children(provider_rows),
                                    )
                                })
                                .when(!reasoning_levels.is_empty(), |element| {
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

    /// 供应商右侧弹出的模型 child-menu。垂直方向按行上/下方剩余空间决定向上
    /// 还是向下展开，水平方向按窗口右缘余量决定向右还是向左弹出。
    #[allow(clippy::too_many_arguments)]
    fn render_provider_submenu(
        &self,
        provider: &ProviderConfig,
        active: Option<&ResolvedModel>,
        room_below_row: Pixels,
        room_above_row: Pixels,
        opens_right: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let opens_down = room_below_row >= px(SUBMENU_MAX_H);
        let max_h = if opens_down {
            px(SUBMENU_MAX_H)
        } else {
            px(SUBMENU_MAX_H).min((room_above_row - px(12.0)).max(px(120.0)))
        };

        let models: Vec<_> = provider.enabled_models().cloned().collect();
        let model_rows: Vec<AnyElement> = models
            .iter()
            .map(|model| {
                let current = active.is_some_and(|resolved| {
                    resolved.provider.id == provider.id && resolved.model.id == model.id
                });
                let provider_id = provider.id.clone();
                let model_id = model.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "model-submenu-model-{}-{}",
                        provider.id, model.id
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_h(px(32.0))
                    .px_2()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(current, |element| element.bg(theme.accent_soft))
                    .when(!current, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_menu_model(&provider_id, &model_id, cx);
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

        div()
            .id(SharedString::from(format!("model-submenu-{}", provider.id)))
            .absolute()
            .when(opens_down, |element| element.top(px(0.0)))
            .when(!opens_down, |element| element.bottom(px(0.0)))
            .when(opens_right, |element| {
                element.left(relative(1.0)).ml(px(6.0))
            })
            .when(!opens_right, |element| {
                element.right(relative(1.0)).mr(px(6.0))
            })
            .w(px(SUBMENU_WIDTH))
            .max_h(max_h)
            .overflow_y_scroll()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow(shadow_float(theme))
            .p_1()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
            .children(model_rows)
            .with_animation(
                SharedString::from(format!("model-submenu-enter-{}", provider.id)),
                Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
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
