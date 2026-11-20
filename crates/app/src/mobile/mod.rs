//! Phone-shaped shell: a near-1:1 port of the DSH-Go mobile client's chrome
//! onto GPUI, rendered inside the desktop window when the user switches the
//! layout to Mobile (Settings → 外观 → 界面布局, `Ctrl+Shift+M`, or
//! `--mobile`).
//!
//! Structure of one frame:
//!
//! ```text
//! window (gradient page + two soft glow blobs)
//! └─ phone column (≤ 420px, full-bleed on narrow windows)
//!    ├─ top bar   [☰] [title card · host] [model pill] [new chat]
//!    ├─ content   chat list + floating composer   (or a settings page)
//!    └─ drawer    scrim + 312px panel (search, filters, grouped sessions)
//! ```
//!
//! Everything here reuses the desktop data model — the same `ChatList`, the
//! same `TextInputView`, the same approval cards — so the two layouts can never
//! disagree about what a session contains. Only the chrome and row rhythm are
//! re-drawn for the phone.

pub(crate) mod chat;
pub(crate) mod composer;
pub(crate) mod pages;

use std::time::{Duration, Instant};

use effibuddy_ui::{
    AccentId, TextInputOptions, TextInputState, TextInputView, Theme, icons, mobile as geo, space,
};
use gpui::{
    AnyElement, App, BoxShadow, Context, Div, Entity, Hsla, IntoElement, ListState, MouseButton,
    SharedString, Window, div, linear_color_stop, linear_gradient, point, prelude::*, px, svg,
};

use effibuddy_mobile::{Filter, Insets, SessionStatus, matches_filter, matches_query};

use crate::app::AppView;
use crate::config::UiLayout;

/// Drawer open/close glide. Matches the reference's 220ms sheet motion.
pub(crate) const DRAWER_MOTION: Duration = Duration::from_millis(220);

#[derive(Clone, Copy)]
struct Slide {
    start: Instant,
    from: f32,
    to: f32,
}

/// A UI action that needs a `Window` but is triggered from a closure that only
/// has a context (sheet rows, message actions). The shell drains the queue at
/// the top of the next frame, where the window is available.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum MobileAction {
    /// Put a quoted copy of `text` in the composer.
    Quote(String),
    /// Start editing a user message (rewind + refill).
    Edit(String),
}

/// Bottom sheets the phone shell can raise. One at a time, like the reference
/// client: the sheet always slides up over the transcript and its scrim.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum MobileSheet {
    /// Provider → model list.
    Model,
    /// Per-conversation actions (pin, archive, delete).
    Session(String),
    /// Quick shell actions: permission mode and layout.
    Actions,
    /// Subagent roster: what is running, what finished, stop button.
    Agents,
    /// One message's actions (copy / quote / edit & resend).
    Message {
        id: String,
        text: String,
        user: bool,
    },
    /// Archived conversations, with restore.
    Archived,
    /// Full-screen image viewer for a tool screenshot.
    Image {
        image: std::sync::Arc<gpui::Image>,
        title: String,
    },
}

/// Mobile-shell UI state that outlives a frame (drawer position, filters,
/// search box, which settings page is open, chat list scroll).
pub(crate) struct MobileState {
    pub(crate) drawer_open: bool,
    /// 0 = closed, 1 = open; interpolated while the slide is in flight.
    pub(crate) drawer_p: f32,
    slide: Option<Slide>,
    /// Frame pump for the drawer glide.
    frame_scheduled: bool,
    pub(crate) filter: Filter,
    pub(crate) search: Entity<TextInputState>,
    pub(crate) search_view: Entity<TextInputView>,
    pub(crate) page: pages::SettingsPage,
    /// Open bottom sheet, if any.
    pub(crate) sheet: Option<MobileSheet>,
    /// 0 = closed, 1 = fully raised.
    pub(crate) sheet_p: f32,
    sheet_slide: Option<Slide>,
    /// Pointer x where an edge swipe started (None when not dragging).
    pub(crate) edge_drag: Option<f32>,
    /// Pointer x where a drag on the open drawer started.
    pub(crate) drawer_drag: Option<f32>,
    /// Virtual list over the transcript (one row per chat item).
    pub(crate) list: ListState,
    /// Item count the list followed last frame, for auto-scroll decisions.
    pub(crate) followed_items: usize,
    /// Set while the transcript should keep pinning to the newest row.
    pub(crate) follow_bottom: bool,
    /// True when the reader scrolled away from the tail, so the shell offers
    /// 「回到底部」 instead of silently yanking them back on every delta.
    pub(crate) show_jump_to_bottom: bool,
    /// Window-dependent action queued by a sheet; drained next frame.
    pub(crate) action: Option<MobileAction>,
    /// Safe-area / keyboard insets. Desktop leaves them zero; the phone build
    /// fills them from the platform. `EFFIBUDDY_MOBILE_INSETS=top,bottom`
    /// overrides them for layout verification.
    pub(crate) insets: Insets,
}

impl MobileState {
    pub(crate) fn new(cx: &mut Context<AppView>) -> Self {
        let search_state = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .placeholder("搜索对话")
                    .multiline(false),
            )
        });
        let search_view = {
            let state = search_state.clone();
            cx.new(|cx| TextInputView::new(state, cx).frameless(true))
        };
        let list = ListState::new(0, gpui::ListAlignment::Top, px(1200.0));
        list.set_estimated_item_height(px(120.0));
        Self {
            drawer_open: false,
            drawer_p: 0.0,
            slide: None,
            frame_scheduled: false,
            filter: Filter::All,
            search: search_state,
            search_view,
            page: pages::SettingsPage::Root,
            sheet: None,
            sheet_p: 0.0,
            sheet_slide: None,
            edge_drag: None,
            drawer_drag: None,
            list,
            followed_items: 0,
            follow_bottom: true,
            show_jump_to_bottom: false,
            action: None,
            insets: env_insets(),
        }
    }

    pub(crate) fn search_query(&self, cx: &App) -> String {
        self.search.read(cx).content().trim().to_string()
    }

    pub(crate) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<AppView>) {
        self.search
            .update(cx, |state, cx| state.clear(window, cx));
    }
}

/// Read `EFFIBUDDY_MOBILE_INSETS=top,bottom` (px). Used to verify safe-area
/// and keyboard padding without a phone; unset means desktop (zero).
fn env_insets() -> Insets {
    std::env::var("EFFIBUDDY_MOBILE_INSETS")
        .ok()
        .and_then(|raw| {
            let mut parts = raw.split(',');
            let top = parts.next()?.trim().parse::<f32>().ok()?;
            let bottom = parts.next().unwrap_or("0").trim().parse::<f32>().ok()?;
            Some(Insets::new(top.max(0.0), bottom.max(0.0)))
        })
        .unwrap_or(Insets::ZERO)
}

/// Advance the drawer glide; returns whether another frame is needed.
fn advance_drawer(slide: &mut Option<Slide>, progress: &mut f32, target: f32) -> bool {
    let now = Instant::now();
    let state = match slide {
        Some(state) if state.to == target => *state,
        _ => Slide {
            start: now,
            from: *progress,
            to: target,
        },
    };
    let t = (now.saturating_duration_since(state.start).as_secs_f32()
        / DRAWER_MOTION.as_secs_f32())
    .min(1.0);
    if t >= 1.0 {
        *progress = state.to;
        *slide = None;
        return false;
    }
    *progress = state.from + (state.to - state.from) * effibuddy_ui::ease_out_cubic(t);
    *slide = Some(state);
    true
}

// --- shared little widgets -------------------------------------------------

/// Frosted chrome surface: translucent fill, 1px hairline, soft lift.
pub(crate) fn glass(theme: &Theme) -> Div {
    div()
        .bg(theme.glass)
        .border_1()
        .border_color(theme.glass_border)
        .shadow(vec![BoxShadow {
            color: theme.shadow,
            offset: point(px(0.0), px(6.0)),
            blur_radius: px(18.0),
            spread_radius: px(0.0),
        }])
}

/// Frosted card: the content surface inside the page.
pub(crate) fn card(theme: &Theme) -> Div {
    div()
        .bg(theme.surface)
        .border_1()
        .border_color(theme.glass_border)
        .rounded(px(geo::RADIUS_CARD))
}

/// A 36–40px circular glass button, the mobile top bar's only chrome shape.
pub(crate) fn round_button(
    theme: &Theme,
    id: impl Into<SharedString>,
    icon: &'static str,
    size: f32,
    icon_size: f32,
) -> gpui::Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(geo::RADIUS_CHIP))
        .bg(theme.glass)
        .border_1()
        .border_color(theme.glass_border)
        .cursor_pointer()
        .hover(|style| style.bg(theme.surface_alt))
        .active(|style| style.opacity(0.72))
        .child(svg().path(icon).size(px(icon_size)).text_color(theme.text))
}

/// Status dot used by the host subtitle and session rows.
pub(crate) fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded(px(geo::RADIUS_CHIP)).bg(color)
}

impl AppView {
    /// Switch the shell layout, persist it and rebuild the palette that layout
    /// uses. Safe to call from either layout's UI.
    pub(crate) fn set_ui_layout(&mut self, layout: UiLayout, cx: &mut Context<Self>) {
        if self.config.ui_layout == layout {
            return;
        }
        self.config.ui_layout = layout;
        // Entering the phone shell closes the desktop-only panels so returning
        // to Desktop cannot restore a drawer-opened settings page.
        self.mobile.drawer_open = false;
        self.mobile.drawer_p = 0.0;
        self.mobile.page = pages::SettingsPage::Root;
        if layout == UiLayout::Mobile {
            self.chat.list_state.reset(0);
        } else {
            self.mobile.list.reset(0);
        }
        self.apply_theme(cx);
        // The shared composer keeps its draft; only the wording of the
        // placeholder follows the layout, as in the reference client.
        let placeholder = if layout == UiLayout::Mobile {
            "给 EffiBuddy 发消息…"
        } else {
            "给 EffiBuddy 一个任务…"
        };
        self.composer_state
            .update(cx, |state, _| state.set_placeholder(placeholder));
        let _ = self.config.save();
        cx.notify();
    }

    pub(crate) fn toggle_ui_layout(&mut self, cx: &mut Context<Self>) {
        let next = if self.config.ui_layout.is_mobile() {
            UiLayout::Desktop
        } else {
            UiLayout::Mobile
        };
        self.set_ui_layout(next, cx);
    }

    /// Rebuild `self.theme` from the persisted brightness + accent, using the
    /// surface language the active layout expects.
    pub(crate) fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let dark = self.theme.is_dark();
        let accent = AccentId::from_id(&self.mobile_accent_id());
        let theme = if self.config.ui_layout.is_mobile() {
            if dark {
                Theme::mobile_dark(accent)
            } else {
                Theme::mobile_light(accent)
            }
        } else if dark {
            Theme::dark()
        } else {
            Theme::light()
        };
        self.theme = theme;
        self.theme.clone().set_global(cx);
    }

    pub(crate) fn mobile_accent(&self) -> AccentId {
        AccentId::from_id(&self.config.mobile_accent)
    }

    fn mobile_accent_id(&self) -> String {
        self.config.mobile_accent.clone()
    }

    pub(crate) fn set_mobile_accent(&mut self, accent: AccentId, cx: &mut Context<Self>) {
        self.config.mobile_accent = accent.id().to_string();
        self.apply_theme(cx);
        let _ = self.config.save();
        cx.notify();
    }

    /// Raise a bottom sheet. Selecting the open sheet's own kind raises it
    /// again (no flicker); another kind swaps content in place.
    pub(crate) fn open_mobile_sheet(&mut self, sheet: MobileSheet, cx: &mut Context<Self>) {
        self.mobile.drawer_open = false;
        self.mobile.sheet = Some(sheet);
        cx.notify();
    }

    /// Raise the message-actions sheet for one message.
    pub(crate) fn open_message_sheet(
        &mut self,
        id: String,
        text: String,
        user: bool,
        cx: &mut Context<Self>,
    ) {
        self.mobile.sheet = Some(MobileSheet::Message { id, text, user });
        cx.notify();
    }

    pub(crate) fn close_mobile_sheet(&mut self, cx: &mut Context<Self>) {
        if self.mobile.sheet.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn toggle_mobile_drawer(&mut self, cx: &mut Context<Self>) {
        self.mobile.drawer_open = !self.mobile.drawer_open;
        if self.mobile.drawer_open {
            self.mobile.sheet = None;
        }
        cx.notify();
    }

    fn close_mobile_drawer(&mut self, cx: &mut Context<Self>) {
        if self.mobile.drawer_open {
            self.mobile.drawer_open = false;
            cx.notify();
        }
    }

    // --- root -------------------------------------------------------------

    pub(crate) fn render_mobile(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = if self.mobile.drawer_open { 1.0 } else { 0.0 };
        let sheet_target = if self.mobile.sheet.is_some() { 1.0 } else { 0.0 };
        let animating =
            advance_drawer(&mut self.mobile.slide, &mut self.mobile.drawer_p, target)
                | advance_drawer(
                    &mut self.mobile.sheet_slide,
                    &mut self.mobile.sheet_p,
                    sheet_target,
                );
        if animating && !self.mobile.frame_scheduled {
            self.mobile.frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.mobile.frame_scheduled = false;
                cx.notify();
            });
        }

        // Drain one window-dependent action queued by a sheet row.
        if let Some(action) = self.mobile.action.take() {
            match action {
                MobileAction::Quote(text) => {
                    let quoted = quote_for_composer(&text);
                    let content = self.composer_state.read(cx).content().to_string();
                    let merged = if content.trim().is_empty() {
                        quoted
                    } else {
                        format!("{quoted}{content}")
                    };
                    self.composer_state
                        .update(cx, |state, cx| state.set_content(merged, window, cx));
                    self.composer_focus_handle(cx).focus(window);
                }
                MobileAction::Edit(id) => self.begin_edit_message(&id, window, cx),
            }
            cx.notify();
        }

        let theme = self.theme.clone();
        let content = if self.settings_open {
            self.render_mobile_settings(window, cx)
        } else {
            self.render_mobile_chat(window, cx)
        };
        let bar = self.render_mobile_bar(&theme, cx);
        let drawer = self.render_mobile_drawer(&theme, window, cx);
        let sheet = self.render_mobile_sheet(&theme, window, cx);

        let page_gradient = linear_gradient(
            180.0,
            linear_color_stop(theme.backdrop_top, 0.0),
            linear_color_stop(theme.backdrop_bottom, 1.0),
        );
        let viewport_w = f32::from(window.viewport_size().width);
        let narrow = viewport_w <= geo::FULL_BLEED_BELOW;

        let insets = self.mobile.insets;
        let mut phone = div()
            .relative()
            .w_full()
            .max_w(px(geo::PHONE_WIDTH))
            .h_full()
            .pt(px(insets.top))
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_size(px(geo::TEXT_BODY))
            .text_color(theme.text)
            .font_family(theme.ui_font());
        if !narrow {
            phone = phone
                .rounded(px(geo::RADIUS_SHEET))
                .border_1()
                .border_color(theme.glass_border)
                .shadow(vec![
                    BoxShadow {
                        color: theme.shadow,
                        offset: point(px(0.0), px(18.0)),
                        blur_radius: px(48.0),
                        spread_radius: px(0.0),
                    },
                    BoxShadow {
                        color: theme.glass_rim,
                        offset: point(px(0.0), px(1.0)),
                        blur_radius: px(2.0),
                        spread_radius: px(0.0),
                    },
                ]);
        }
        phone = phone
            .child(bar)
            .child(content)
            .child(drawer)
            .child(sheet);

        div()
            .key_context("EffiBuddy")
            .size_full()
            .relative()
            .flex()
            .justify_center()
            .bg(page_gradient)
            .text_color(theme.text)
            .text_size(px(geo::TEXT_BODY))
            .font_family(theme.ui_font())
            .on_action(cx.listener(|this, _: &crate::app::ToggleMobileLayout, _, cx| {
                this.toggle_ui_layout(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::app::NewSession, window, cx| {
                this.new_session(window, cx);
                this.close_mobile_drawer(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::app::ToggleTheme, _, cx| {
                this.toggle_theme(cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::ToggleSidebar, _, cx| {
                this.toggle_mobile_drawer(cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::ToggleSettings, window, cx| {
                this.toggle_mobile_settings(window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::FocusComposer, window, cx| {
                this.composer_focus_handle(cx).focus(window);
            }))
            .on_action(cx.listener(|this, _: &crate::app::OpenCommandPalette, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::app::StopRun, window, cx| {
                this.handle_escape(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::app::CyclePermissionMode, _, cx| {
                this.cycle_permission_mode(cx)
            }))
            .child(phone)
            .when_some(self.render_model_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when_some(self.render_session_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when_some(self.render_project_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when(self.palette_open, |element| {
                element.child(self.render_palette(window, cx))
            })
            .when(
                self.undo_dialog.is_some() || self.edit_conflict.is_some(),
                |element| element.child(self.render_restore_dialog(cx)),
            )
            .into_any_element()
    }

    /// Toggle the settings page from the mobile shell.
    pub(crate) fn toggle_mobile_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open {
            self.settings_open = false;
            self.mobile.page = pages::SettingsPage::Root;
            cx.notify();
        } else {
            self.open_settings(window, cx);
        }
    }

    // --- top bar ----------------------------------------------------------

    fn render_mobile_bar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let title = self
            .active_shared()
            .map(|shared| effibuddy_agent_core::lock_session(&shared).display_title())
            .unwrap_or_else(|| "新对话".to_string());
        let running = self.active_run.is_some();
        let host = if running { "运行中" } else { "本机" };
        let host_color = if running {
            theme.success
        } else {
            theme.text_faint
        };

        let resolved = self.active_resolved_model();
        let model_label = resolved.model_label().to_string();
        let dot_color = if self.models.demo_mode(self.config.use_demo, &resolved) {
            theme.warning
        } else if resolved.live {
            theme.success
        } else {
            theme.warning
        };

        let mut bar = div()
            .flex_none()
            .h(px(geo::BAR_HEIGHT))
            .px(px(geo::GUTTER))
            .flex()
            .items_center()
            .gap(px(space::SM))
            .child(
                round_button(theme, "mobile-menu", icons::MENU, 36.0, 17.0)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_mobile_drawer(cx))),
            )
            .child(
                // Title card: bold title over the host status, exactly like the
                // reference's [title · ● 本机] pill.
                glass(theme)
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(38.0))
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .rounded(px(12.0))
                    .px(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(geo::TEXT_TITLE))
                                    .line_height(px(22.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .child(dot(host_color, 5.0))
                                    .child(
                                        div()
                                            .text_size(px(geo::TEXT_META))
                                            .line_height(px(14.0))
                                            .text_color(theme.text_faint)
                                            .child(host),
                                    ),
                            ),
                    ),
            )
            .child(
                // Model pill: [● model ⌄]
                div()
                    .id("mobile-model-pill")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(theme.glass)
                    .border_1()
                    .border_color(theme.glass_border)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_alt))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_mobile_sheet(MobileSheet::Model, cx);
                    }))
                    .child(dot(dot_color, 6.0))
                    .child(
                        div()
                            .max_w(px(112.0))
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(model_label),
                    )
                    .child(
                        svg()
                            .path(icons::CHEVRON_DOWN)
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                    ),
            )
            .child(
                round_button(theme, "mobile-new-chat", icons::EDIT, 36.0, 16.0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_session(window, cx);
                        this.close_mobile_drawer(cx);
                    })),
            );

        bar = bar.on_mouse_down(MouseButton::Left, |_, _, _| {});
        bar.into_any_element()
    }

    // --- drawer -----------------------------------------------------------

    fn render_mobile_drawer(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.mobile.drawer_p.clamp(0.0, 1.0);
        if p <= 0.001 {
            return div().into_any_element();
        }
        let width = geo::DRAWER_WIDTH;
        let offset = -width * (1.0 - p);
        let scrim = theme.scrim.opacity(0.55 * p);

        let mut panel = div()
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w(px(width))
            .bg(theme.glass_strong)
            .border_r_1()
            .border_color(theme.glass_border)
            .shadow(vec![BoxShadow {
                color: theme.shadow,
                offset: point(px(6.0), px(0.0)),
                blur_radius: px(28.0),
                spread_radius: px(0.0),
            }])
            .child(self.render_drawer_header(theme, window, cx))
            .child(self.render_drawer_filter_bar(theme, cx))
            .child(self.render_drawer_list(theme, window, cx))
            .child(self.render_drawer_footer(theme, cx));

        panel = panel
            .left(px(offset))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                this.mobile.drawer_drag = Some(f32::from(event.position.x));
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.mobile.drawer_drag = None;
                cx.notify();
            }))
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    // Swipe the panel left to close it, mirroring the gesture
                    // that opened it.
                    if let Some(start) = this.mobile.drawer_drag {
                        if f32::from(event.position.x) - start < -48.0 {
                            this.mobile.drawer_drag = None;
                            this.mobile.drawer_open = false;
                            cx.notify();
                        }
                    }
                }),
            );

        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .id("mobile-drawer-scrim")
                    .absolute()
                    .inset_0()
                    .bg(scrim)
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.mobile.drawer_open = false;
                        cx.notify();
                    })),
            )
            .child(panel)
            .into_any_element()
    }

    fn render_drawer_header(
        &mut self,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query_empty = self.mobile.search.read(cx).content().trim().is_empty();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::MD))
            .px(px(geo::GUTTER))
            .pt(px(14.0))
            .pb(px(space::MD))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(space::SM))
                            .child(
                                div()
                                    .size(px(26.0))
                                    .rounded(px(geo::RADIUS_CARD))
                                    .bg(theme.accent_soft)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        svg()
                                            .path(icons::CHAT)
                                            .size(px(14.0))
                                            .text_color(theme.accent),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("EffiBuddy"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("drawer-new")
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .h(px(30.0))
                                    .px(px(11.0))
                                    .rounded(px(geo::RADIUS_CHIP))
                                    .bg(theme.accent)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.accent_hover))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_session(window, cx);
                                        this.mobile.drawer_open = false;
                                        cx.notify();
                                    }))
                                    .child(
                                        svg()
                                            .path(icons::PLUS)
                                            .size(px(12.0))
                                            .text_color(theme.on_accent),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .text_color(theme.on_accent)
                                            .child("新对话"),
                                    ),
                            )
                            .child(
                                round_button(theme, "drawer-settings", icons::SETTINGS, 32.0, 15.0)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_mobile_settings(window, cx);
                                        this.mobile.drawer_open = false;
                                        cx.notify();
                                    })),
                            )
                            .when(!query_empty, |element| {
                                element.child(
                                    round_button(theme, "drawer-clear", icons::X, 32.0, 14.0)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mobile.clear_search(window, cx);
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(geo::RADIUS_CARD))
                    .bg(theme.surface_alt)
                    .border_1()
                    .border_color(theme.glass_border)
                    .child(
                        svg()
                            .path(icons::SEARCH)
                            .size(px(13.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(13.0))
                            .child(self.mobile.search_view.clone()),
                    ),
            )
            .into_any_element()
    }

    fn render_drawer_filter_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(geo::GUTTER))
            .pb(px(space::MD));
        let archived = self.archived_sessions.len();
        for filter in Filter::ALL {
            let selected = self.mobile.filter == filter;
            row = row.child(
                div()
                    .id(SharedString::from(format!("drawer-filter-{}", filter.label())))
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(if selected {
                        theme.text
                    } else {
                        theme.surface_alt
                    })
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mobile.filter = filter;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if selected {
                                theme.bg
                            } else {
                                theme.text_muted
                            })
                            .child(filter.label()),
                    ),
            );
        }
        if archived > 0 {
            row = row.child(div().flex_1()).child(
                div()
                    .id("drawer-archived")
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_mobile_sheet(MobileSheet::Archived, cx);
                    }))
                    .child(
                        svg()
                            .path(icons::ARCHIVE)
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(format!("归档 {archived}")),
                    ),
            );
        }
        row.into_any_element()
    }

    fn render_drawer_list(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.mobile.search_query(cx);
        let rows = self.mobile_session_rows(&query);
        let mut column = div()
            .id("mobile-drawer-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(space::SM))
            .pb(px(space::MD));

        if rows.is_empty() {
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(space::SM))
                    .py(px(48.0))
                    .child(
                        svg()
                            .path(icons::SEARCH)
                            .size(px(20.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.text_faint)
                            .child(if query.is_empty() {
                                "还没有会话"
                            } else {
                                "没有匹配的对话"
                            }),
                    ),
            );
            return column.into_any_element();
        }

        let mut last_group: Option<&'static str> = None;
        for row in rows {
            if last_group != Some(row.group) {
                last_group = Some(row.group);
                column = column.child(
                    div()
                        .px(px(space::SM))
                        .pt(px(space::MD))
                        .pb(px(6.0))
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_faint)
                        .child(row.group),
                );
            }
            column = column.child(self.render_drawer_row(theme, row, window, cx));
        }
        column.into_any_element()
    }

    fn render_drawer_row(
        &self,
        theme: &Theme,
        row: DrawerRow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.session_is_active(&row.id);
        let id = SharedString::from(format!("drawer-session-{}", row.id));
        let session_id = row.id.clone();
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(space::SM))
            .h(px(geo::LIST_ROW_HEIGHT))
            .px(px(10.0))
            .mb(px(2.0))
            .rounded(px(geo::RADIUS_CARD))
            .bg(if selected {
                theme.surface_alt
            } else {
                theme.glass.opacity(0.0)
            })
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(&session_id, window, cx);
                this.mobile.drawer_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(13.5))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(row.title),
            )
            .when(row.running, |element| {
                element.child(
                    div()
                        .flex_none()
                        .size(px(6.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .bg(theme.success),
                )
            })
            .when(row.waiting_approval, |element| {
                element.child(
                    div()
                        .flex_none()
                        .size(px(6.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .bg(theme.warning),
                )
            })
            .when(row.unread, |element| {
                element.child(
                    div()
                        .flex_none()
                        .size(px(6.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .bg(theme.accent),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("drawer-more-{}", row.id)))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click({
                        let session_id = row.id.clone();
                        cx.listener(move |this, _, _, cx| {
                            this.open_mobile_sheet(
                                MobileSheet::Session(session_id.clone()),
                                cx,
                            );
                        })
                    })
                    .child(
                        svg()
                            .path(icons::MORE)
                            .size(px(13.0))
                            .text_color(theme.text_faint),
                    ),
            )
            .into_any_element()
    }

    fn render_drawer_footer(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (label, tone) = if self.active_run.is_some() {
            ("运行中", theme.success)
        } else {
            ("待命", theme.text_faint)
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(space::SM))
            .px(px(geo::GUTTER))
            .py(px(space::MD))
            .border_t_1()
            .border_color(theme.glass_border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .child(
                        div()
                            .size(px(28.0))
                            .rounded(px(geo::RADIUS_CARD))
                            .bg(theme.accent_soft)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                svg()
                                    .path(icons::TERMINAL)
                                    .size(px(13.0))
                                    .text_color(theme.accent),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("本机"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .child(dot(tone, 5.0))
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(theme.text_faint)
                                            .child(label),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        round_button(theme, "drawer-theme", if theme.is_dark() {
                            icons::SUN
                        } else {
                            icons::MOON
                        }, 32.0, 14.0)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
                    )
                    .child(
                        round_button(theme, "drawer-actions", icons::MORE, 32.0, 14.0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_mobile_sheet(MobileSheet::Actions, cx);
                            })),
                    )
                    .child(
                        round_button(theme, "drawer-prefs", icons::SETTINGS, 32.0, 14.0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_mobile_settings(window, cx);
                                this.mobile.drawer_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    // --- bottom sheets ----------------------------------------------------

    /// The sheet host: scrim + panel that slides up from the bottom edge of
    /// the phone column. `sheet_p` interpolates so opening and closing share
    /// one motion curve with the drawer.
    fn render_mobile_sheet(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(sheet) = self.mobile.sheet.clone() else {
            return div().into_any_element();
        };
        let p = self.mobile.sheet_p.clamp(0.0, 1.0);
        if p <= 0.001 {
            return div().into_any_element();
        }
        let offset = (1.0 - p) * 320.0;
        let title = match &sheet {
            MobileSheet::Model => "切换模型".to_string(),
            MobileSheet::Session(id) => self
                .sessions
                .iter()
                .find(|summary| &summary.id == id)
                .map(|summary| summary.title.clone())
                .unwrap_or_else(|| "会话".to_string()),
            MobileSheet::Actions => "快捷操作".to_string(),
            MobileSheet::Agents => "子代理".to_string(),
            MobileSheet::Message { .. } => "消息操作".to_string(),
            MobileSheet::Archived => "归档会话".to_string(),
            MobileSheet::Image { title, .. } => title.clone(),
        };

        let body: AnyElement = match &sheet {
            MobileSheet::Model => self.render_model_sheet(theme, cx),
            MobileSheet::Session(id) => {
                let id = id.clone();
                self.render_session_sheet(theme, &id, window, cx)
            }
            MobileSheet::Actions => self.render_actions_sheet(theme, window, cx),
            MobileSheet::Agents => self.render_agents_sheet(theme, cx),
            MobileSheet::Message { id, text, user } => {
                let (id, text, user) = (id.clone(), text.clone(), *user);
                self.render_message_sheet(theme, &id, &text, user, window, cx)
            }
            MobileSheet::Archived => self.render_archived_sheet(theme, cx),
            MobileSheet::Image { image, .. } => {
                let image = image.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM))
                    .child(
                        div()
                            .w_full()
                            .rounded(px(geo::RADIUS_CARD))
                            .border_1()
                            .border_color(theme.glass_border)
                            .bg(theme.code_bg)
                            .p(px(4.0))
                            .child(
                                gpui::img(image)
                                    .w_full()
                                    .max_h(px(420.0))
                                    .object_fit(gpui::ObjectFit::Contain),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child("点右上角 × 关闭；再次点缩略图可重开。"),
                    )
                    .into_any_element()
            }
        };

        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .id("mobile-sheet-scrim")
                    .absolute()
                    .inset_0()
                    .bg(theme.scrim.opacity(0.5 * p))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.mobile.sheet = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(-offset))
                    .max_h(px(if matches!(sheet, MobileSheet::Image { .. }) {
                        620.0
                    } else {
                        520.0
                    }))
                    .flex()
                    .flex_col()
                    .rounded_t(px(geo::RADIUS_SHEET))
                    .bg(theme.glass_strong)
                    .border_t_1()
                    .border_color(theme.glass_border)
                    .shadow(vec![BoxShadow {
                        color: theme.shadow,
                        offset: point(px(0.0), px(-10.0)),
                        blur_radius: px(32.0),
                        spread_radius: px(0.0),
                    }])
                    .child(
                        // Drag handle + title, the affordance that says "this
                        // is a sheet, tap outside to dismiss".
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(space::SM))
                            .pt(px(space::SM))
                            .pb(px(space::MD))
                            .child(
                                div()
                                    .w(px(38.0))
                                    .h(px(4.0))
                                    .rounded(px(geo::RADIUS_CHIP))
                                    .bg(theme.border_strong),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .px(px(geo::GUTTER))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(15.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .child(
                                        round_button(theme, "sheet-close", icons::X, 30.0, 13.0)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.mobile.sheet = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("mobile-sheet-body")
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .px(px(geo::GUTTER))
                            .pb(px(space::XXL))
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    /// Provider → model list, one row per enabled model.
    fn render_model_sheet(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let resolved = self.active_resolved_model();
        let current_provider = resolved.provider.id.clone();
        let current_model = resolved.model.id.clone();
        let providers: Vec<(String, String, Vec<(String, String, usize)>)> = self
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
                    provider
                        .models
                        .iter()
                        .filter(|model| model.enabled)
                        .map(|model| {
                            (
                                model.id.clone(),
                                if model.name.trim().is_empty() {
                                    model.id.clone()
                                } else {
                                    model.name.clone()
                                },
                                model.context_window,
                            )
                        })
                        .collect(),
                )
            })
            .collect();

        let mut column = div().flex().flex_col().gap(px(space::MD));
        for (provider_id, provider_name, models) in providers {
            column = column.child(
                card(theme)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(space::MD))
                            .pt(px(space::SM))
                            .pb(px(space::XS))
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_faint)
                            .child(provider_name),
                    )
                    .children(models.into_iter().map(|(model_id, label, context)| {
                        let selected =
                            provider_id == current_provider && model_id == current_model;
                        let pid = provider_id.clone();
                        let mid = model_id.clone();
                        div()
                            .id(SharedString::from(format!("sheet-model-{}-{}", pid, mid)))
                            .flex()
                            .items_center()
                            .gap(px(space::SM))
                            .h(px(44.0))
                            .px(px(space::MD))
                            .border_t_1()
                            .border_color(theme.glass_border)
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_model_selection(&pid, &mid, None, cx);
                                this.mobile.sheet = None;
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
                                    .child(format!("{}k", context / 1000)),
                            )
                            .when(selected, |element| {
                                element.child(
                                    svg()
                                        .path(icons::CHECK)
                                        .size(px(13.0))
                                        .text_color(theme.accent),
                                )
                            })
                    })),
            );
        }
        column.into_any_element()
    }

    fn render_session_sheet(
        &mut self,
        theme: &Theme,
        id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pinned = self
            .sessions
            .iter()
            .find(|summary| summary.id == id)
            .is_some_and(|summary| summary.pinned);
        let session_id = id.to_string();
        let pin_label = if pinned { "取消置顶" } else { "置顶" };
        let pin_id = session_id.clone();
        let archive_id = session_id.clone();
        let delete_id = session_id.clone();
        let mut column = card(theme).flex().flex_col();
        column = column
            .child(sheet_action_row(
                theme,
                "sheet-pin",
                icons::PIN,
                pin_label,
                move |this, cx| {
                    this.toggle_pin(&pin_id, cx);
                    this.mobile.sheet = None;
                },
                cx,
            ))
            .child(sheet_action_row(
                theme,
                "sheet-archive",
                icons::ARCHIVE,
                "归档",
                move |this, cx| {
                    this.archive_session(&archive_id, cx);
                    this.mobile.sheet = None;
                },
                cx,
            ))
            .child(sheet_action_row(
                theme,
                "sheet-delete",
                icons::TRASH,
                "删除",
                move |this, cx| {
                    this.delete_session(&delete_id, cx);
                    this.mobile.sheet = None;
                },
                cx,
            ));
        column.into_any_element()
    }

    fn render_actions_sheet(
        &mut self,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use effibuddy_agent_core::tool::PermissionMode;
        let mode = self.permission_mode();
        let mut mode_card = card(theme).flex().flex_col().child(
            div()
                .px(px(space::MD))
                .pt(px(space::SM))
                .pb(px(space::XS))
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_faint)
                .child("权限模式"),
        );
        for (value, label) in [
            (PermissionMode::Default, "默认询问"),
            (PermissionMode::AcceptEdits, "自动编辑"),
            (PermissionMode::Plan, "计划模式"),
            (PermissionMode::FullAccess, "完全访问"),
        ] {
            let selected = mode == value;
            mode_card = mode_card.child(
                div()
                    .id(SharedString::from(format!("sheet-perm-{label}")))
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .h(px(42.0))
                    .px(px(space::MD))
                    .border_t_1()
                    .border_color(theme.glass_border)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_permission_mode(value, cx);
                        this.mobile.sheet = None;
                    }))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(label),
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

        let column = div()
            .flex()
            .flex_col()
            .gap(px(space::MD))
            .child(mode_card)
            .child(
                card(theme)
                    .flex()
                    .flex_col()
                    .child(sheet_action_row(
                        theme,
                        "sheet-theme",
                        if theme.is_dark() {
                            icons::SUN
                        } else {
                            icons::MOON
                        },
                        if theme.is_dark() { "切到浅色" } else { "切到深色" },
                        |this, cx| {
                            this.toggle_theme(cx);
                            this.mobile.sheet = None;
                        },
                        cx,
                    ))
                    .child(sheet_action_row(
                        theme,
                        "sheet-layout",
                        icons::PANEL_RIGHT,
                        "回到桌面视图",
                        |this, cx| {
                            this.set_ui_layout(UiLayout::Desktop, cx);
                            this.mobile.sheet = None;
                        },
                        cx,
                    )),
            );
        column.into_any_element()
    }

    /// Message actions. Phones have no hover, so the copy/edit/quote entry
    /// lives behind an explicit ⋯ on the message instead of a hover toolbar.
    fn render_message_sheet(
        &mut self,
        theme: &Theme,
        message_id: &str,
        text: &str,
        user: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copy_text = text.to_string();
        let quote_text = text.to_string();
        let edit_id = message_id.to_string();
        let mut column = div().flex().flex_col().gap(px(space::MD)).child(
            card(theme)
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(space::MD))
                .child(
                    div()
                        .max_h(px(96.0))
                        .overflow_hidden()
                        .text_size(px(11.5))
                        .line_height(px(17.0))
                        .text_color(theme.text_muted)
                        .child(text.to_string()),
                ),
        );
        column = column.child(
            card(theme)
                .flex()
                .flex_col()
                .child(sheet_action_row(
                    theme,
                    "msg-copy",
                    icons::COPY,
                    "复制文字",
                    move |this, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()));
                        this.mobile.sheet = None;
                    },
                    cx,
                ))
                .child(sheet_action_row(
                    theme,
                    "msg-quote",
                    icons::CHAT,
                    "引用到输入框",
                    move |this, _cx| {
                        this.mobile.sheet = None;
                        this.mobile.action = Some(MobileAction::Quote(quote_text.clone()));
                    },
                    cx,
                ))
                .when(user, |element| {
                    element.child(sheet_action_row(
                        theme,
                        "msg-edit",
                        icons::EDIT,
                        "编辑并重发",
                        move |this, _cx| {
                            this.mobile.sheet = None;
                            this.mobile.action = Some(MobileAction::Edit(edit_id.clone()));
                        },
                        cx,
                    ))
                }),
        );
        column.into_any_element()
    }

    /// Archived conversations with a restore action — the phone equivalent of
    /// Settings → 归档.
    fn render_archived_sheet(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<(String, String, String)> = self
            .archived_sessions
            .iter()
            .map(|summary| {
                (
                    summary.id.clone(),
                    if summary.title.trim().is_empty() {
                        "未命名对话".to_string()
                    } else {
                        summary.title.clone()
                    },
                    crate::chat::message_time_label(summary.updated_at),
                )
            })
            .collect();
        if rows.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .py(px(space::XXL))
                .child(
                    svg()
                        .path(icons::ARCHIVE)
                        .size(px(20.0))
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_faint)
                        .child("没有归档的会话"),
                )
                .into_any_element();
        }
        let mut column = card(theme).flex().flex_col();
        for (id, title, when) in rows {
            let restore_id = id.clone();
            column = column.child(
                div()
                    .id(SharedString::from(format!("sheet-restore-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .h(px(48.0))
                    .px(px(space::MD))
                    .border_t_1()
                    .border_color(theme.glass_border)
                    .hover(|style| style.bg(theme.hover))
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
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(when),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("sheet-restore-btn-{restore_id}")))
                            .h(px(26.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .rounded(px(geo::RADIUS_CHIP))
                            .bg(theme.surface_alt)
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.restore_archived_session(&restore_id, cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child("恢复"),
                            ),
                    ),
            );
        }
        column.into_any_element()
    }

    /// Subagent roster. On a phone the desktop side pane has no room, so the
    /// shell surfaces the same information as a sheet: who is running, what
    /// each child was asked to do, and a stop button per live child.
    fn render_agents_sheet(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut tasks: Vec<_> = self
            .subagent_tasks
            .iter()
            .map(|task| {
                (
                    task.agent_id.clone(),
                    task.name.clone(),
                    task.profile.clone(),
                    task.description.clone(),
                    task.status,
                    task.tool_uses,
                    task.color,
                )
            })
            .collect();
        // Live children first, then newest-terminal; the sheet is a status
        // view, not a history browser.
        tasks.sort_by_key(|(_, _, _, _, status, _, _)| status.is_terminal());

        if tasks.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .py(px(space::XXL))
                .child(
                    svg()
                        .path(icons::SPARKLES)
                        .size(px(20.0))
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_faint)
                        .child("当前没有子代理"),
                )
                .into_any_element();
        }

        let mut column = div().flex().flex_col().gap(px(space::SM));
        for (agent_id, name, profile, description, status, tool_uses, color) in tasks {
            let tone = match status {
                effibuddy_agent_core::SubagentStatus::Running => theme.accent,
                effibuddy_agent_core::SubagentStatus::Completed => theme.success,
                effibuddy_agent_core::SubagentStatus::Failed => theme.danger,
                _ => theme.text_faint,
            };
            let stop_id = agent_id.clone();
            column = column.child(
                card(theme)
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .p(px(space::MD))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(dot(tone, 6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .child(if name.trim().is_empty() {
                                        profile.clone()
                                    } else {
                                        name.clone()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(tone)
                                    .child(status.label()),
                            )
                            .when(
                                status == effibuddy_agent_core::SubagentStatus::Running,
                                |element| {
                                    element.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "sheet-stop-agent-{stop_id}"
                                            )))
                                            .h(px(24.0))
                                            .px(px(8.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(geo::RADIUS_CHIP))
                                            .bg(theme.danger_soft)
                                            .cursor_pointer()
                                            .hover(|style| style.opacity(0.85))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.stop_agent(&stop_id, cx);
                                                cx.notify();
                                            }))
                                            .child(
                                                div()
                                                    .text_size(px(10.0))
                                                    .text_color(theme.danger)
                                                    .child("停止"),
                                            ),
                                    )
                                },
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(if description.trim().is_empty() {
                                profile.clone()
                            } else {
                                description.clone()
                            }),
                    )
                    .when(tool_uses > 0, |element| {
                        element.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(format!("{tool_uses} 次工具调用 · {profile}")),
                        )
                    }),
            );
        }
        column.into_any_element()
    }

    // --- session projection ----------------------------------------------

    /// Sessions that pass the active drawer filter + search query, grouped by
    /// day bucket in the reference's order (今天 / 昨天 / 本周 / 更早).
    fn mobile_session_rows(&self, query: &str) -> Vec<DrawerRow> {
        let mut rows: Vec<DrawerRow> = self
            .sessions
            .iter()
            .filter(|summary| {
                if !matches_query(query, &summary.title, &summary.id) {
                    return false;
                }
                let status = self.session_row_status(&summary.id);
                let shared = SessionStatus {
                    running: status.running || self.run_active_for(&summary.id),
                    waiting_approval: status.waiting_approval,
                    unread: summary.unread,
                };
                matches_filter(self.mobile.filter, &shared)
            })
            .map(|summary| {
                let status = self.session_row_status(&summary.id);
                DrawerRow {
                    id: summary.id.clone(),
                    title: if summary.title.trim().is_empty() {
                        "未命名对话".to_string()
                    } else {
                        summary.title.clone()
                    },
                    group: session_group_label(summary.updated_at),
                    running: status.running,
                    waiting_approval: status.waiting_approval,
                    unread: summary.unread,
                }
            })
            .collect();

        // Pinned first inside their bucket, then newest first — the same order
        // the desktop sidebar uses, minus projects.
        rows.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.title.cmp(&b.title)));
        rows
    }
}

/// Prefix a message body as a Markdown quote for the composer.
fn quote_for_composer(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines().take(6) {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// One tappable row inside a sheet. The handler receives the view and context
/// so sheet rows can call the same app methods the desktop menus call.
fn sheet_action_row(
    theme: &Theme,
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    action: impl Fn(&mut AppView, &mut Context<AppView>) + 'static,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let action = std::rc::Rc::new(action);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(space::MD))
        .h(px(46.0))
        .px(px(space::MD))
        .border_t_1()
        .border_color(theme.glass_border)
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(
            svg()
                .path(icon)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(13.0))
                .text_color(theme.text)
                .child(label),
        )
        .into_any_element()
}

/// One row of the mobile drawer's session list.
pub(crate) struct DrawerRow {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) group: &'static str,
    pub(crate) running: bool,
    pub(crate) waiting_approval: bool,
    pub(crate) unread: bool,
}

/// Day bucket for a session row. Buckets are static strings so the drawer can
/// compare them without allocating.
pub(crate) fn session_group_label(updated_at: chrono::DateTime<chrono::Utc>) -> &'static str {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let local = Local.from_utc_datetime(&updated_at.naive_utc());
    let today = Local::now().date_naive();
    let date = local.date_naive();
    if date == today {
        "今天"
    } else if date == today - Duration::try_days(1).unwrap_or_default() {
        "昨天"
    } else if date > today - Duration::try_days(7).unwrap_or_default() {
        "本周"
    } else if local.year() == Local::now().year() {
        "更早"
    } else {
        "更早"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_ids_survive_a_round_trip() {
        for accent in AccentId::ALL {
            assert_eq!(AccentId::from_id(accent.id()), *accent);
        }
    }

    #[test]
    fn drawer_group_labels_are_bucketed_by_day() {
        use chrono::{Duration, Utc};
        let now = Utc::now();
        assert_eq!(session_group_label(now), "今天");
        assert_eq!(session_group_label(now - Duration::days(2)), "本周");
        assert_eq!(session_group_label(now - Duration::days(30)), "更早");
    }
}
