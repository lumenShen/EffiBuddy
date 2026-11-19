//! Mobile settings: a big-title page stack with grouped cards, matching the
//! reference client's settings rhythm (one screen per topic, rows of 44px,
//! swatches for the accent, a footer note under every group).
//!
//! The pages write the same `AppConfig` / session metadata the desktop
//! settings write, so switching layouts never loses a choice.

use effibuddy_ui::{AccentId, Theme, icons, mobile as geo, radius, space};
use gpui::{
    AnyElement, ClickEvent, Context, Div, IntoElement, SharedString, Window, div, prelude::*, px,
    svg,
};

use crate::app::AppView;
use crate::config::UiLayout;
use crate::mobile::{card, round_button};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SettingsPage {
    #[default]
    Root,
    Appearance,
    General,
    Models,
    About,
}

impl SettingsPage {
    fn title(self) -> &'static str {
        match self {
            SettingsPage::Root => "设置",
            SettingsPage::Appearance => "外观",
            SettingsPage::General => "通用",
            SettingsPage::Models => "模型",
            SettingsPage::About => "关于",
        }
    }
}

impl AppView {
    pub(crate) fn render_mobile_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let page = self.mobile.page;

        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(space::SM))
            .px(px(geo::GUTTER))
            .py(px(space::SM))
            .child(
                round_button(
                    &theme,
                    "mobile-settings-back",
                    if page == SettingsPage::Root {
                        icons::X
                    } else {
                        icons::CHEVRON_LEFT
                    },
                    36.0,
                    16.0,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.mobile.page == SettingsPage::Root {
                        this.settings_open = false;
                        cx.notify();
                    } else {
                        this.mobile.page = SettingsPage::Root;
                        cx.notify();
                    }
                })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(geo::TEXT_TITLE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(page.title()),
            );

        let body = div()
            .id("mobile-settings-scroll")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(space::LG))
            .px(px(geo::GUTTER))
            .pb(px(space::XXL));

        let content = match page {
            SettingsPage::Root => self.mobile_settings_root(&theme, cx),
            SettingsPage::Appearance => self.mobile_settings_appearance(&theme, cx),
            SettingsPage::General => self.mobile_settings_general(&theme, cx),
            SettingsPage::Models => {
                let _ = window;
                self.mobile_settings_models(&theme, cx)
            }
            SettingsPage::About => self.mobile_settings_about(&theme),
        };

        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(header)
            .child(body.child(content))
            .into_any_element()
    }

    // --- root -------------------------------------------------------------

    fn mobile_settings_root(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let accent = self.mobile_accent();
        let resolved = self.active_resolved_model();
        let mut column = div().flex().flex_col().gap(px(space::LG));

        column = column.child(section_label(theme, "偏好"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .child(self.settings_nav_row(
                    theme,
                    icons::SUN,
                    "外观",
                    format!("{} · {}", brightness_label(theme.is_dark()), accent.label()),
                    SettingsPage::Appearance,
                    cx,
                ))
                .child(self.settings_nav_row(
                    theme,
                    icons::SETTINGS,
                    "通用",
                    "权限模式、演示模式与界面布局".to_string(),
                    SettingsPage::General,
                    cx,
                ))
                .child(self.settings_nav_row(
                    theme,
                    icons::SPARKLES,
                    "模型",
                    resolved.model_label().to_string(),
                    SettingsPage::Models,
                    cx,
                )),
        );

        column = column.child(section_label(theme, "会话与数据"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .child(self.settings_action_row(
                    theme,
                    icons::CHAT,
                    "会话",
                    format!("{} 个活动 · {} 个归档", self.sessions.len(), self.archived_sessions.len()),
                    "mobile-open-sessions",
                    cx.listener(|this, _, _, cx| {
                        this.settings_open = false;
                        this.mobile.drawer_open = true;
                        cx.notify();
                    }),
                ))
                .child(self.settings_action_row(
                    theme,
                    icons::FOLDER,
                    "工作区",
                    self.config.workspace().display().to_string(),
                    "mobile-open-workspace",
                    cx.listener(|this, _, _, cx| {
                        if let Some(workspace) = this.config.workspace.clone() {
                            let _ = open_in_file_manager(&workspace);
                            cx.notify();
                        }
                    }),
                )),
        );

        column = column.child(section_label(theme, "其他"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .child(self.settings_nav_row(
                    theme,
                    icons::HEART,
                    "关于",
                    format!("EffiBuddy {}", crate::update::current_version()),
                    SettingsPage::About,
                    cx,
                ))
                .child(self.settings_action_row(
                    theme,
                    icons::SUN,
                    "切换明暗",
                    "也可以按 Ctrl+Shift+T".to_string(),
                    "mobile-settings-toggle-theme",
                    cx.listener(|this, _, _, cx| this.toggle_theme(cx)),
                )),
        );

        column.into_any_element()
    }

    fn settings_nav_row(
        &self,
        theme: &Theme,
        icon: &'static str,
        title: impl Into<SharedString>,
        subtitle: String,
        page: SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title: SharedString = title.into();
        let id = SharedString::from(format!("mobile-settings-nav-{title}"));
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(space::MD))
            .h(px(56.0))
            .px(px(space::LG))
            .border_b_1()
            .border_color(theme.glass_border)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.mobile.page = page;
                cx.notify();
            }))
            .child(icon_tile(theme, icon))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text_faint)
                            .child(subtitle),
                    ),
            )
            .child(
                svg()
                    .path(icons::CHEVRON_RIGHT)
                    .size(px(13.0))
                    .text_color(theme.text_faint),
            )
            .into_any_element()
    }

    fn settings_action_row(
        &self,
        theme: &Theme,
        icon: &'static str,
        title: impl Into<SharedString>,
        subtitle: String,
        id: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(space::MD))
            .h(px(56.0))
            .px(px(space::LG))
            .border_b_1()
            .border_color(theme.glass_border)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(on_click)
            .child(icon_tile(theme, icon))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(theme.text)
                            .child(title.into()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text_faint)
                            .child(subtitle),
                    ),
            )
            .into_any_element()
    }

    // --- appearance -------------------------------------------------------

    fn mobile_settings_appearance(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dark = theme.is_dark();
        let accent = self.mobile_accent();
        let mut column = div().flex().flex_col().gap(px(space::LG));

        column = column.child(section_label(theme, "外观"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(space::MD))
                .p(px(space::LG))
                .child(segmented(
                    theme,
                    "mobile-brightness",
                    &[
                        ("浅色", !dark, BrightnessChoice::Light as u8),
                        ("深色", dark, BrightnessChoice::Dark as u8),
                    ],
                    {
                        let entity = cx.entity().downgrade();
                        move |choice, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                let want_dark = choice == BrightnessChoice::Dark as u8;
                                if this.theme.is_dark() != want_dark {
                                    this.toggle_theme(cx);
                                }
                            });
                        }
                    },
                ))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("手机视图使用晨雾渐变与磨砂玻璃；桌面视图保持中性纸面。"),
                ),
        );

        column = column.child(section_label(theme, "配色"));
        let mut swatch_row = div().flex().flex_wrap().gap(px(space::MD));
        for preset in AccentId::ALL {
            let colors = preset.colors();
            let selected = *preset == accent;
            let id = SharedString::from(format!("accent-{}", preset.id()));
            swatch_row = swatch_row.child(
                div()
                    .id(id)
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(5.0))
                    .w(px(64.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_mobile_accent(*preset, cx);
                    }))
                    .child(
                        div()
                            .size(px(34.0))
                            .rounded(px(geo::RADIUS_CHIP))
                            .bg(if dark {
                                colors.seed_dark()
                            } else {
                                colors.seed()
                            })
                            .when(selected, |element| {
                                element
                                    .border_2()
                                    .border_color(theme.text)
                                    .p(px(0.0))
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(selected, |element| {
                                element.child(
                                    svg()
                                        .path(icons::CHECK)
                                        .size(px(14.0))
                                        .text_color(colors.on_seed_for(dark)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_faint
                            })
                            .child(preset.label()),
                    ),
            );
        }
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(space::MD))
                .p(px(space::LG))
                .child(swatch_row)
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("强调色只用于主按钮、发送键、选中态与链接；页面与文字保持中性。"),
                ),
        );

        column = column.child(section_label(theme, "界面布局"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(space::MD))
                .p(px(space::LG))
                .child(segmented(
                    theme,
                    "mobile-layout",
                    &[
                        ("手机视图", self.config.ui_layout.is_mobile(), 1),
                        ("桌面视图", !self.config.ui_layout.is_mobile(), 0),
                    ],
                    {
                        let entity = cx.entity().downgrade();
                        move |choice, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                let layout = if choice == 1 {
                                    UiLayout::Mobile
                                } else {
                                    UiLayout::Desktop
                                };
                                this.set_ui_layout(layout, cx);
                            });
                        }
                    },
                ))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("Ctrl+Shift+M 可在两种布局之间切换；手机视图在窄窗口下自动铺满。"),
                ),
        );

        column.into_any_element()
    }

    // --- general ----------------------------------------------------------

    fn mobile_settings_general(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use effibuddy_agent_core::tool::PermissionMode;

        let mode = self.permission_mode();
        let demo = self.config.use_demo;
        let mut column = div().flex().flex_col().gap(px(space::LG));

        column = column.child(section_label(theme, "行为"));
        let mut modes = card(theme).flex().flex_col();
        for (value, label, hint) in [
            (PermissionMode::Default, "默认询问", "写文件、跑命令前先问你"),
            (PermissionMode::AcceptEdits, "自动编辑", "文件改动自动放行，其它仍询问"),
            (PermissionMode::Plan, "计划模式", "只读：任何写操作都会被拒绝"),
            (PermissionMode::FullAccess, "完全访问", "全部放行，仅建议在沙箱里用"),
        ] {
            let selected = mode == value;
            modes = modes.child(self.radio_row(
                theme,
                SharedString::from(format!("perm-{}", label)),
                label,
                hint.to_string(),
                selected,
                cx.listener(move |this, _, _, cx| {
                    this.set_permission_mode(value, cx);
                }),
            ));
        }
        column = column.child(modes);

        column = column.child(section_label(theme, "会话"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .child(self.toggle_row(
                    theme,
                    "mobile-toggle-demo",
                    "强制离线 Demo",
                    "即使配置了 Key 也使用本地演示 Provider",
                    demo,
                    cx.listener(|this, _, _, cx| {
                        this.config.use_demo = !this.config.use_demo;
                        let _ = this.config.save();
                        cx.notify();
                    }),
                ))
                .child(self.toggle_row(
                    theme,
                    "mobile-toggle-updates",
                    "检查更新",
                    "启动时查询 GitHub Releases",
                    self.config.update_check_enabled,
                    cx.listener(|this, _, _, cx| {
                        this.config.update_check_enabled = !this.config.update_check_enabled;
                        let _ = this.config.save();
                        cx.notify();
                    }),
                )),
        );

        column = column.child(section_label(theme, "工作区"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(space::LG))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(self.config.workspace().display().to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child("新会话与工具沙箱的根目录；桌面视图里可以更换。"),
                ),
        );

        column.into_any_element()
    }

    // --- models -----------------------------------------------------------

    fn mobile_settings_models(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let resolved = self.active_resolved_model();
        let current_provider = resolved.provider.id.clone();
        let current_model = resolved.model.id.clone();
        let demo_mode = self.models.demo_mode(self.config.use_demo, &resolved);
        let providers: Vec<_> = self
            .models
            .file()
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .map(|provider| {
                (
                    provider.id.clone(),
                    if provider.name.trim().is_empty() {
                        provider.id.clone()
                    } else {
                        provider.name.clone()
                    },
                    provider.models.clone(),
                )
            })
            .collect();

        let mut column = div().flex().flex_col().gap(px(space::LG));
        column = column.child(section_label(theme, "当前模型"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(space::LG))
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(resolved.model_label().to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(format!(
                            "{} · {} · {} tokens 上下文{}",
                            resolved.provider.name,
                            resolved.model.id,
                            resolved.context_window(),
                            if demo_mode { " · Demo 模式" } else { "" }
                        )),
                ),
        );

        column = column.child(section_label(theme, "可用模型"));
        for (provider_id, provider_name, models) in providers {
            let mut group = card(theme).flex().flex_col().child(
                div()
                    .px(px(space::LG))
                    .pt(px(space::MD))
                    .pb(px(space::XS))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child(provider_name),
            );
            let enabled: Vec<_> = models.into_iter().filter(|model| model.enabled).collect();
            if enabled.is_empty() {
                group = group.child(
                    div()
                        .px(px(space::LG))
                        .pb(px(space::MD))
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("没有启用的模型"),
                );
            }
            for model in enabled {
                let selected =
                    provider_id == current_provider && model.id == current_model;
                let pid = provider_id.clone();
                let mid = model.id.clone();
                let label = if model.name.trim().is_empty() {
                    model.id.clone()
                } else {
                    model.name.clone()
                };
                group = group.child(
                    div()
                        .id(SharedString::from(format!("model-{}-{}", pid, mid)))
                        .flex()
                        .items_center()
                        .gap(px(space::SM))
                        .h(px(46.0))
                        .px(px(space::LG))
                        .border_t_1()
                        .border_color(theme.glass_border)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_model_selection(&pid, &mid, None, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(if selected {
                                    theme.accent
                                } else {
                                    theme.text
                                })
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(format!(
                                    "{}k",
                                    model.context_window / 1000
                                )),
                        )
                        .when(selected, |element| {
                            element.child(
                                svg()
                                    .path(icons::CHECK)
                                    .size(px(13.0))
                                    .text_color(theme.accent),
                            )
                        }),
                );
            }
            column = column.child(group);
        }

        column.into_any_element()
    }

    // --- about ------------------------------------------------------------

    fn mobile_settings_about(&self, theme: &Theme) -> AnyElement {
        let mut column = div().flex().flex_col().gap(px(space::LG));
        column = column.child(section_label(theme, "EffiBuddy"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(10.0))
                .p(px(space::LG))
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(format!("EffiBuddy {}", crate::update::current_version())),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(
                            "Rust + GPUI 的本地优先桌面 Agent。手机视图复刻 DSH Go 的界面与手感，\
                             复用同一套会话、工具与权限模型。",
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("MIT License · 界面参考 dsh-go（AGPL-3.0，仅作设计参照）"),
                ),
        );
        column = column.child(section_label(theme, "快捷键"));
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(8.0))
                .p(px(space::LG))
                .child(shortcut_line(theme, "Ctrl+Shift+M", "桌面 / 手机视图"))
                .child(shortcut_line(theme, "Ctrl+Shift+T", "明暗主题"))
                .child(shortcut_line(theme, "Ctrl+N", "新建对话"))
                .child(shortcut_line(theme, "Ctrl+B", "会话抽屉"))
                .child(shortcut_line(theme, "Esc", "停止当前任务")),
        );
        column.into_any_element()
    }

    // --- row builders -----------------------------------------------------

    fn radio_row(
        &self,
        theme: &Theme,
        id: SharedString,
        title: &'static str,
        hint: String,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(space::MD))
            .h(px(52.0))
            .px(px(space::LG))
            .border_b_1()
            .border_color(theme.glass_border)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(on_click)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(hint),
                    ),
            )
            .child(
                div()
                    .size(px(18.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .border_1()
                    .border_color(if selected {
                        theme.accent
                    } else {
                        theme.border_strong
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(selected, |element| {
                        element.child(
                            div()
                                .size(px(9.0))
                                .rounded(px(geo::RADIUS_CHIP))
                                .bg(theme.accent),
                        )
                    }),
            )
            .into_any_element()
    }

    fn toggle_row(
        &self,
        theme: &Theme,
        id: &'static str,
        title: &'static str,
        hint: &'static str,
        on: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(space::MD))
            .h(px(56.0))
            .px(px(space::LG))
            .border_b_1()
            .border_color(theme.glass_border)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(on_click)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(hint),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(42.0))
                    .h(px(24.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(if on { theme.accent } else { theme.disabled_bg })
                    .p(px(2.0))
                    .flex()
                    .items_center()
                    .when(on, |element| element.justify_end())
                    .child(div().size(px(20.0)).rounded(px(geo::RADIUS_CHIP)).bg(
                        if on {
                            theme.on_accent
                        } else {
                            theme.surface
                        },
                    )),
            )
            .into_any_element()
    }
}

// --- little helpers -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BrightnessChoice {
    Light = 0,
    Dark = 1,
}

fn brightness_label(dark: bool) -> &'static str {
    if dark { "深色" } else { "浅色" }
}

fn section_label(theme: &Theme, label: &'static str) -> AnyElement {
    div()
        .pt(px(space::SM))
        .text_size(px(11.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(label)
        .into_any_element()
}

fn icon_tile(theme: &Theme, icon: &'static str) -> Div {
    div()
        .flex_none()
        .size(px(30.0))
        .rounded(px(radius::MD))
        .bg(theme.accent_soft)
        .flex()
        .items_center()
        .justify_center()
        .child(svg().path(icon).size(px(14.0)).text_color(theme.accent))
}

fn shortcut_line(theme: &Theme, keys: &'static str, label: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .text_size(px(10.5))
                .font_family(theme.mono_font())
                .text_color(theme.text_faint)
                .child(keys),
        )
        .into_any_element()
}

/// Two-or-more segment pill; `on_click` receives the selected segment value.
fn segmented(
    theme: &Theme,
    id: &'static str,
    options: &[(&'static str, bool, u8)],
    on_click: impl Fn(u8, &mut gpui::App) + 'static,
) -> AnyElement {
    let on_click = std::rc::Rc::new(on_click);
    let mut row = div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(geo::RADIUS_CHIP))
        .bg(theme.surface_alt);
    for (index, (label, selected, value)) in options.iter().enumerate() {
        let value = *value;
        let selected = *selected;
        let handler = on_click.clone();
        let segment_id = SharedString::from(format!("{id}-{index}"));
        row = row.child(
            div()
                .id(segment_id)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .h(px(30.0))
                .rounded(px(geo::RADIUS_CHIP))
                .bg(if selected {
                    theme.surface
                } else {
                    theme.glass.opacity(0.0)
                })
                .when(selected, |element| {
                    element.border_1().border_color(theme.glass_border)
                })
                .cursor_pointer()
                .hover(|style| style.opacity(0.9))
                .on_click(move |_, _, cx| handler(value, cx))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(*label),
                ),
        );
    }
    row.into_any_element()
}

fn open_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot open {}: {error}", path.display()))
}
