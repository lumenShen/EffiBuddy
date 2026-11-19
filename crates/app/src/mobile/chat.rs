//! Mobile transcript: bubbles, folded process rows, tool rows and the run
//! status, drawn for a 420px column.
//!
//! The data is the desktop's `ChatList` projection — nothing is re-derived
//! here, so a message that renders one way in the desktop pane renders the
//! same content on the phone. Each chat item is one virtual row; the markdown
//! body keeps using the document cache keyed by `text_key`, so scrolling a
//! long transcript costs hashing per row, not re-parsing.

use std::collections::HashMap;
use std::rc::Rc;

use effibuddy_agent_core::TodoStatus;
use effibuddy_ui::{
    Theme, icons, mobile as geo, radius, space,
};
use gpui::{
    AnyElement, Context, IntoElement, ListOffset, ListState, SharedString, Window, div, img,
    prelude::*, px, svg,
};

use crate::app::AppView;
use crate::chat::{
    AssistantItem, ChatItem, ChatItemKind, ChatViewState, RunSnapshot, TurnItem, assistant_texts,
    format_duration_ms, latest_reasoning_line, markdown_interaction, message_time_label,
    tool_action_label, tool_detail, tool_icon, turn_body, turn_expanded_with, with_stream_caret,
};
use crate::mobile::card;

/// Maximum bubble width: the reference keeps a bubble under ~78% of the
/// column so a long message still reads as a message, not an article.
const BUBBLE_MAX_W: f32 = 306.0;

impl AppView {
    pub(crate) fn render_mobile_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Reveal + scroll pumps, same contract as the desktop chat: while the
        // typewriter or a glide is running, request the next frame.
        if self.chat.tick() && !self.chat.reveal_frame_scheduled {
            self.chat.reveal_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.chat.reveal_frame_scheduled = false;
                cx.notify();
            });
        }

        let theme = self.theme.clone();
        let Some(shared) = self.active_shared() else {
            return div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(self.render_mobile_no_session(&theme, cx))
                .child(self.render_mobile_composer(window, cx))
                .into_any_element();
        };

        if self.chat_dirty {
            let approval = self.pending_approvals.first();
            let run = RunSnapshot {
                running: self.active_run.is_some(),
                started_at: self.active_run.as_ref().map(|run| run.started_at),
                waiting_approval: approval.is_some(),
                approval_tool: approval.map(|pending| match &pending.agent_id {
                    Some(_) => format!("子代理 · {}", pending.request.tool_name),
                    None => pending.request.tool_name.clone(),
                }),
                last_error: self.last_error.clone(),
                notice: self.notice.clone(),
                retry: self.retry_status.clone(),
            };
            {
                let session = effibuddy_agent_core::lock_session(&shared);
                self.chat.rebuild(&session, &run);
            }
            self.chat_dirty = false;
        }

        let (message_count, current_todo) = {
            let session = effibuddy_agent_core::lock_session(&shared);
            (
                session.messages.len(),
                session
                    .todos
                    .iter()
                    .find(|todo| todo.status == TodoStatus::InProgress)
                    .map(|todo| todo.content.clone()),
            )
        };
        let show_empty = message_count == 0
            && self.last_error.is_none()
            && self.notice.is_none()
            && self.pending_approvals.is_empty();

        let agent_cards = self.agent_cards();
        let entity = cx.entity().downgrade();
        let mut view = self.chat.prepare_frame(
            self.active_run.is_some(),
            true,
            &self.expanded_reasoning,
            &self.expanded_tools,
            &self.expanded_turns,
            &self.collapsed_turns,
            &agent_cards,
            &entity,
        );
        view.jump_flash = self.jump_flash.clone();
        let streaming = view.stream.is_some();
        let item_count = view.items.len();
        let view = Rc::new(view);

        // One virtual row per item, plus a trailing spacer so "scroll to the
        // last row" lands at the true bottom instead of above the composer.
        let row_count = item_count + 1;
        let list_state: ListState = self.mobile.list.clone();
        let followed_before = self.mobile.followed_items;
        if list_state.item_count() != row_count {
            list_state.reset(row_count);
        }
        // Geometry from the previous frame: if the reader has scrolled away
        // from the tail, stop yanking them back while the answer streams in.
        // 120px is roughly one comfortable line above the composer.
        let max_offset = f32::from(list_state.max_offset_for_scrollbar().height);
        let offset = -f32::from(list_state.scroll_px_offset_for_scrollbar().y);
        let near_bottom = (max_offset - offset) < 120.0 || followed_before == 0;
        if (row_count != followed_before || streaming) && near_bottom {
            list_state.scroll_to(ListOffset {
                item_ix: row_count.saturating_sub(1),
                offset_in_item: px(0.0),
            });
            self.mobile.followed_items = row_count;
        }
        self.mobile.show_jump_to_bottom = !near_bottom && !show_empty;

        let row_theme = theme.clone();
        let row_entity = entity.clone();
        let row_view = view.clone();
        let rows = gpui::list(list_state, move |index, _window, _cx| {
            if index >= row_view.items.len() {
                return div().h(px(space::MD)).into_any_element();
            }
            let item = row_view.items[index].clone();
            render_mobile_row(&item, &row_view, &row_theme, &row_entity).into_any_element()
        })
        .size_full();

        let mut body = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col();
        // Touch affordances the desktop gets from hover/menus:
        //  * tapping the transcript dismisses the soft keyboard (nothing else
        //    to focus on a phone), and
        //  * a right-swipe that starts near the left edge opens the drawer.
        let gesture_entity = cx.entity().downgrade();
        body = body.child(
            div()
                .id("mobile-transcript")
                .flex_1()
                .min_h(px(0.0))
                .px(px(geo::GUTTER))
                .pt(px(space::MD))
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    let x = f32::from(event.position.x);
                    let _ = gesture_entity.update(cx, |this, _| {
                        this.mobile.edge_drag = (x < 36.0).then_some(x);
                    });
                    window.blur();
                })
                .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.mobile.edge_drag = None;
                    cx.notify();
                }))
                .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    if let Some(start) = this.mobile.edge_drag {
                        if f32::from(event.position.x) - start > 48.0 {
                            this.mobile.edge_drag = None;
                            this.mobile.drawer_open = true;
                            cx.notify();
                        }
                    }
                }))
                .child(if show_empty {
                    self.render_mobile_empty(&theme, cx)
                } else {
                    rows.into_any_element()
                }),
        );

        // Approval + queue + todo dock sit right above the composer, exactly
        // where a thumb expects the decision.
        if let Some(pending) = self.pending_approvals.first().cloned() {
            let card = self.render_mobile_approval(&pending.request, cx);
            let extra = self.pending_approvals.len().saturating_sub(1);
            body = body.child(
                div()
                    .px(px(geo::GUTTER))
                    .pb(px(space::SM))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(card)
                    .when(extra > 0, |element| {
                        element.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(format!("还有 {extra} 个待确认请求")),
                        )
                    }),
            );
        }
        if let Some(todo) = current_todo {
            body = body.child(div().px(px(geo::GUTTER)).pb(px(space::SM)).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::SM))
                    .h(px(32.0))
                    .px(px(12.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(theme.glass)
                    .border_1()
                    .border_color(theme.glass_border)
                    .child(
                        svg()
                            .path(icons::CLOCK)
                            .size(px(12.0))
                            .text_color(theme.accent),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(todo),
                    ),
            ));
        }

        if self.mobile.show_jump_to_bottom {
            let list_state = self.mobile.list.clone();
            let target = self.mobile.list.item_count().saturating_sub(1);
            body = body.child(
                div().flex().justify_center().pb(px(space::XS)).child(
                    div()
                        .id("mobile-jump-bottom")
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .h(px(30.0))
                        .px(px(12.0))
                        .rounded(px(geo::RADIUS_CHIP))
                        .bg(theme.glass)
                        .border_1()
                        .border_color(theme.glass_border)
                        .shadow(vec![gpui::BoxShadow {
                            color: theme.shadow,
                            offset: gpui::point(px(0.0), px(4.0)),
                            blur_radius: px(12.0),
                            spread_radius: px(0.0),
                        }])
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.surface_alt))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            list_state.scroll_to(ListOffset {
                                item_ix: target,
                                offset_in_item: px(0.0),
                            });
                            this.chat.pin_to_bottom();
                            cx.notify();
                        }))
                        .child(
                            svg()
                                .path(icons::CHEVRON_DOWN)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child("回到底部"),
                        ),
                ),
            );
        }

        body.child(self.render_mobile_composer(window, cx))
            .into_any_element()
    }

    fn render_mobile_no_session(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(space::MD))
            .px(px(geo::GUTTER))
            .child(
                div()
                    .size(px(52.0))
                    .rounded(px(geo::RADIUS_CARD))
                    .bg(theme.accent_soft)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .path(icons::CHAT)
                            .size(px(24.0))
                            .text_color(theme.accent),
                    ),
            )
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("没有活动会话"),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_faint)
                    .child("点左上角 ☰ 打开会话列表，或新建一个对话。"),
            )
            .child(
                div()
                    .id("mobile-new-from-empty")
                    .h(px(34.0))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .rounded(px(geo::RADIUS_CHIP))
                    .bg(theme.accent)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.accent_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx)))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.on_accent)
                            .child("新建对话"),
                    ),
            )
            .into_any_element()
    }

    fn render_mobile_empty(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let suggestions: [(&'static str, &'static str); 3] = [
            ("写代码", "帮我实现一个小功能并说明改动"),
            ("查资料", "读一下这个仓库，总结目录结构"),
            ("整理", "把这段对话的过程整理成要点"),
        ];
        let mut column = div()
            .id("mobile-empty")
            .flex()
            .flex_col()
            .gap(px(space::LG))
            .pt(px(space::XXL));
        column = column.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(space::SM))
                .child(
                    div()
                        .size(px(54.0))
                        .rounded(px(geo::RADIUS_CARD))
                        .bg(theme.accent_soft)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            svg()
                                .path(icons::SPARKLES)
                                .size(px(24.0))
                                .text_color(theme.accent),
                        ),
                )
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("今天想做点什么？"),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_faint)
                        .child("EffiBuddy 会读文件、跑命令、开浏览器，也会问你要授权。"),
                ),
        );
        for (title, prompt) in suggestions {
            let prompt_owned = prompt.to_string();
            column = column.child(
                card(theme)
                    .id(SharedString::from(format!("mobile-suggest-{title}")))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(space::SM))
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_alt))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.mobile_prefill(prompt_owned.clone(), window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_faint)
                                    .child(prompt),
                            ),
                    )
                    .child(
                        svg()
                            .path(icons::CHEVRON_RIGHT)
                            .size(px(13.0))
                            .text_color(theme.text_faint),
                    ),
            );
        }
        column.into_any_element()
    }
}

impl AppView {
    /// Approval card in its phone form: what the tool wants to do, the risk,
    /// and three thumb-sized choices stacked full width (approve once, approve
    /// for the session, deny). The desktop card's inline buttons are too small
    /// to hit and its argument dump is unreadable at 420px.
    fn render_mobile_approval(
        &self,
        request: &effibuddy_agent_core::ApprovalRequest,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use effibuddy_agent_core::ApprovalDecision;

        let theme = &self.theme;
        let summary = request.summary.lines().next().unwrap_or("").trim().to_string();
        let detail = crate::chat::tool_detail(&effibuddy_agent_core::ToolCall {
            id: request.call_id.clone(),
            name: request.tool_name.clone(),
            arguments: request.arguments.clone(),
            argument_parse_error: None,
        });
        div()
            .w_full()
            .rounded(px(geo::RADIUS_CARD))
            .border_1()
            .border_color(theme.warning)
            .bg(theme.warning_soft)
            .p(px(space::MD))
            .flex()
            .flex_col()
            .gap(px(space::SM))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        svg()
                            .path(icons::ALERT)
                            .size(px(13.0))
                            .text_color(theme.warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(format!("需要确认 · {}", request.tool_name)),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(theme.text_muted)
                    .child(if summary.is_empty() { detail.clone() } else { summary }),
            )
            .when(!detail.is_empty(), |element| {
                element.child(
                    div()
                        .truncate()
                        .text_size(px(10.5))
                        .font_family(theme.mono_font())
                        .text_color(theme.text_faint)
                        .child(detail),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("mobile-approve-once")
                            .h(px(geo::TOUCH))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(12.0))
                            .bg(theme.accent)
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.accent_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.respond_approval(ApprovalDecision::Approve, cx)
                            }))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.on_accent)
                                    .child("允许本次"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("mobile-approve-session")
                                    .flex_1()
                                    .h(px(40.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(12.0))
                                    .bg(theme.surface)
                                    .border_1()
                                    .border_color(theme.glass_border)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.surface_alt))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.respond_approval(
                                            ApprovalDecision::ApproveForSession,
                                            cx,
                                        )
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.text_muted)
                                            .child("本轮都允许"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("mobile-approve-deny")
                                    .flex_1()
                                    .h(px(40.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(12.0))
                                    .bg(theme.surface)
                                    .border_1()
                                    .border_color(theme.glass_border)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.surface_alt))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.respond_approval(ApprovalDecision::Deny, cx)
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.danger)
                                            .child("拒绝"),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

// --- rows -----------------------------------------------------------------

fn render_mobile_row(
    item: &Rc<ChatItem>,
    view: &ChatViewState,
    theme: &Theme,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    match &item.kind {
        ChatItemKind::User {
            text,
            steered,
            attachments,
        } => render_user_bubble(&item.id, text, *steered, attachments, theme, entity),
        ChatItemKind::Assistant(assistant) => render_assistant_block(assistant, view, theme, entity),
        ChatItemKind::Turn(turn) => render_turn_block(turn, view, theme, entity),
        ChatItemKind::RunStatus(status) => render_run_status(status, theme),
        ChatItemKind::Error { text } => render_notice(theme, text, theme.danger, icons::ALERT),
        ChatItemKind::Notice { text } => {
            render_notice(theme, text, theme.text_muted, icons::ALERT)
        }
        ChatItemKind::System { text } => render_notice(theme, text, theme.text_faint, icons::CLOCK),
        ChatItemKind::TaskNotification { summary } => {
            render_notice(theme, summary, theme.text_faint, icons::CLOCK)
        }
        ChatItemKind::AgentMessage { meta, text } => {
            render_agent_message(theme, &meta.from_name, text)
        }
        ChatItemKind::Empty => div().into_any_element(),
    }
}

fn render_user_bubble(
    message_id: &str,
    text: &SharedString,
    steered: bool,
    attachments: &[effibuddy_agent_core::UserAttachment],
    theme: &Theme,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    let mut bubble = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .max_w(px(BUBBLE_MAX_W))
        .rounded(px(geo::RADIUS_BUBBLE))
        .bg(theme.accent_bubble)
        .px(px(14.0))
        .py(px(10.0));

    if !attachments.is_empty() {
        for attachment in attachments {
            bubble = bubble.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(8.0))
                    .bg(theme.surface.opacity(0.6))
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(
                        svg()
                            .path(if attachment.kind == effibuddy_agent_core::AttachmentKind::Image {
                                icons::IMAGE
                            } else {
                                icons::FILE
                            })
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                    )
                    .child(attachment.name.clone()),
            );
        }
    }

    bubble = bubble.child(
        div()
            .text_size(px(geo::TEXT_BODY))
            .line_height(px(23.0))
            .text_color(theme.bubble_text)
            .child(text.clone()),
    );
    let action_id = message_id.to_string();
    let action_text = text.to_string();
    let action_entity = entity.clone();
    bubble = bubble.child(
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(space::SM))
            .when(steered, |element| {
                element
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child("回合中插话"),
                    )
            })
            .child(
                div()
                    .id(SharedString::from(format!("mobile-user-actions-{message_id}")))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.0))
                    .rounded(px(geo::RADIUS_CHIP))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click(move |_, _, cx| {
                        let _ = action_entity.update(cx, |this, cx| {
                            this.open_message_sheet(
                                action_id.clone(),
                                action_text.clone(),
                                true,
                                cx,
                            );
                        });
                    })
                    .child(
                        svg()
                            .path(icons::MORE)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    ),
            ),
    );
    div()
        .w_full()
        .flex()
        .justify_end()
        .mb(px(space::LG))
        .child(bubble)
        .into_any_element()
}

fn render_assistant_block(
    assistant: &Rc<AssistantItem>,
    view: &ChatViewState,
    theme: &Theme,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    let texts = assistant_texts(assistant, view);
    let active = texts.active() && view.stream.is_some();
    let reasoning = texts.reasoning().to_string();
    let body = if active {
        with_stream_caret(texts.text())
    } else {
        texts.text().to_string()
    };
    drop(texts);

    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(space::SM))
        .mb(px(space::LG));

    if !reasoning.is_empty() {
        column = column.child(render_thinking_row(
            &assistant.message.id,
            &reasoning,
            assistant.reasoning_duration,
            active,
            view,
            theme,
            entity,
        ));
    }
    if !assistant.message.tool_calls.is_empty() {
        column = column.child(render_tool_stack(
            &assistant.message.tool_calls,
            view,
            theme,
        ));
    }
    if !body.trim().is_empty() {
        let interaction = markdown_interaction(view.markdown.base_dir(), entity);
        column = column.child(render_markdown_body(
            theme,
            &body,
            assistant.text_key,
            &interaction,
            active,
        ));
    }
    column = column.child(render_meta_row(assistant, theme, active, None, entity));
    column.into_any_element()
}

fn render_turn_block(
    turn: &TurnItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    let expanded = turn_expanded_with(
        turn,
        &view.collapsed_turns,
        &view.expanded_turns,
        view.running,
        view.stream
            .as_ref()
            .map(|stream| stream.message_id.as_str()),
    );
    let body_member = turn_body(turn, view);
    let texts = assistant_texts(body_member, view);
    let active = texts.active() && view.stream.is_some();
    let body = if active {
        with_stream_caret(texts.text())
    } else {
        texts.text().to_string()
    };
    drop(texts);

    let summary = {
        let tools = turn.tool_calls + turn.agent_calls;
        if active {
            "正在处理…".to_string()
        } else if tools > 0 {
            format!(
                "已完成 · {tools} 次工具调用 · {} 条消息",
                turn.members.len()
            )
        } else {
            format!("已完成 · {} 条消息", turn.members.len())
        }
    };

    let turn_id = turn.id.clone();
    let toggle_id = turn_id.clone();
    let toggle_entity = entity.clone();
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(space::SM))
        .mb(px(space::LG))
        .child(
            div()
                .id(SharedString::from(format!("mobile-turn-{}", turn.id)))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let _ = toggle_entity.update(cx, |this, cx| {
                        this.toggle_turn(&toggle_id, expanded);
                        cx.notify();
                    });
                })
                .child(
                    svg()
                        .path(icons::CIRCLE_CHECK)
                        .size(px(13.0))
                        .text_color(if turn.any_error {
                            theme.danger
                        } else {
                            theme.text_faint
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(geo::TEXT_META))
                        .text_color(if active {
                            theme.text_muted
                        } else {
                            theme.text_faint
                        })
                        .child(summary),
                )
                .child(
                    svg()
                        .path(if expanded {
                            icons::CHEVRON_DOWN
                        } else {
                            icons::CHEVRON_RIGHT
                        })
                        .size(px(12.0))
                        .text_color(theme.text_faint),
                ),
        );

    if expanded {
        for member in &turn.members {
            let member_texts = assistant_texts(member, view);
            let reasoning = member_texts.reasoning().to_string();
            let member_active = member_texts.active() && view.stream.is_some();
            drop(member_texts);
            if !reasoning.is_empty() {
                column = column.child(div().pl(px(18.0)).child(render_thinking_row(
                    &member.message.id,
                    &reasoning,
                    member.reasoning_duration,
                    member_active,
                    view,
                    theme,
                    entity,
                )));
            }
            if !member.message.tool_calls.is_empty() {
                column = column.child(
                    div()
                        .pl(px(18.0))
                        .child(render_tool_stack(&member.message.tool_calls, view, theme)),
                );
            }
        }
    }

    if !body.trim().is_empty() {
        let interaction = markdown_interaction(view.markdown.base_dir(), entity);
        column = column.child(div().pt(px(2.0)).child(render_markdown_body(
            theme,
            &body,
            body_member.text_key,
            &interaction,
            active,
        )));
    }
    column = column.child(render_meta_row(body_member, theme, active, Some(turn), entity));
    div().w_full().child(column).into_any_element()
}

fn render_thinking_row(
    message_id: &str,
    reasoning: &str,
    duration: Option<std::time::Duration>,
    active: bool,
    view: &ChatViewState,
    theme: &Theme,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    let expanded = view.expanded_reasoning.contains(message_id);
    let label = if active {
        "正在思考…".to_string()
    } else {
        match duration {
            Some(duration) => format!("思考 · {}", format_duration_ms(duration.as_millis() as u64)),
            None => "思考".to_string(),
        }
    };
    let preview = latest_reasoning_line(reasoning);
    let id = message_id.to_string();
    let toggle_entity = entity.clone();
    let mut column = div()
        .id(SharedString::from(format!("mobile-think-{}", message_id)))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            let _ = toggle_entity.update(cx, |this, cx| {
                this.toggle_reasoning(&id);
                cx.notify();
            });
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .child(
                    svg()
                        .path(icons::SPARKLES)
                        .size(px(12.0))
                        .text_color(if active { theme.accent } else { theme.text_faint }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(geo::TEXT_META))
                        .text_color(theme.text_faint)
                        .child(if preview.is_empty() {
                            label.clone()
                        } else {
                            format!("{label} · {preview}")
                        }),
                )
                .child(
                    svg()
                        .path(if expanded {
                            icons::CHEVRON_DOWN
                        } else {
                            icons::CHEVRON_RIGHT
                        })
                        .size(px(11.0))
                        .text_color(theme.text_faint),
                ),
        );
    if expanded {
        column = column.child(
            div()
                .mt(px(2.0))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(theme.glass_border)
                .bg(theme.code_bg)
                .p(px(10.0))
                .text_size(px(12.0))
                .line_height(px(19.0))
                .text_color(theme.text_muted)
                .child(reasoning.to_string()),
        );
    }
    column.into_any_element()
}

fn render_tool_stack(
    calls: &[effibuddy_agent_core::ToolCall],
    view: &ChatViewState,
    theme: &Theme,
) -> AnyElement {
    let results: &HashMap<String, Rc<effibuddy_agent_core::ToolResult>> = view.results.as_ref();
    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .rounded(px(radius::MD))
        .bg(theme.tool_bg)
        .border_1()
        .border_color(theme.glass_border)
        .px(px(10.0))
        .py(px(8.0));
    for call in calls {
        let result = results.get(&call.id).map(|result| result.as_ref());
        column = column.child(render_tool_row(call, result, theme, view.running));
        if let Some(result) = result
            && !result.is_error
            && matches!(call.name.as_str(), "write_file" | "edit_file")
        {
            column = column.child(render_file_card(call, view, theme));
        }
        if let Some(result) = result {
            // Screenshots and image reads are the one tool output a phone
            // cannot read as text: draw them in the column at phone width
            // instead of the desktop's fixed 420×280 card.
            for index in 0..result.images.len() {
                if let Some(image) = crate::chat::tool_image(view, &call.id, result, index) {
                    column = column.child(
                        img(image)
                            .w_full()
                            .max_h(px(260.0))
                            .mt(px(4.0))
                            .rounded(px(radius::SM))
                            .border_1()
                            .border_color(theme.glass_border)
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                }
            }
        }
    }
    column.into_any_element()
}

/// A file a tool produced, as a tappable card: name, folder and an "open"
/// action. This is the phone form of the desktop's "交付了 N 个文件" block.
fn render_file_card(
    call: &effibuddy_agent_core::ToolCall,
    view: &ChatViewState,
    theme: &Theme,
) -> AnyElement {
    let raw = call
        .arguments
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if raw.is_empty() {
        return div().into_any_element();
    }
    let path = {
        let candidate = std::path::PathBuf::from(&raw);
        if candidate.is_absolute() {
            candidate
        } else {
            view.markdown.base_dir().join(candidate)
        }
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| raw.clone());
    let dir = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default();
    let verb = if call.name == "write_file" {
        "已写入"
    } else {
        "已修改"
    };
    div()
        .mt(px(4.0))
        .rounded(px(radius::SM))
        .border_1()
        .border_color(theme.glass_border)
        .bg(theme.surface)
        .flex()
        .items_center()
        .gap(px(space::SM))
        .px(px(8.0))
        .py(px(6.0))
        .child(
            div()
                .flex_none()
                .size(px(28.0))
                .rounded(px(8.0))
                .bg(theme.accent_soft)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    svg()
                        .path(icons::FILE)
                        .size(px(13.0))
                        .text_color(theme.accent),
                ),
        )
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
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(format!("{verb} · {dir}")),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("tool-open-{}", call.id)))
                .flex_none()
                .h(px(24.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .rounded(px(geo::RADIUS_CHIP))
                .bg(theme.surface_alt)
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    if let Err(error) = crate::app::open_in_file_manager(&path) {
                        eprintln!("[mobile] {error}");
                    }
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child("打开"),
                ),
        )
        .into_any_element()
}

fn render_tool_row(
    call: &effibuddy_agent_core::ToolCall,
    result: Option<&effibuddy_agent_core::ToolResult>,
    theme: &Theme,
    running: bool,
) -> AnyElement {
    let failed = result.is_some_and(|result| result.is_error);
    let pending = result.is_none() && running;
    let label = tool_action_label(&call.name, result, running);
    let detail = tool_detail(call);
    let duration = result
        .and_then(|result| result.duration_ms)
        .map(|ms| format_duration_ms(ms));
    let tone = if failed {
        theme.danger
    } else if pending {
        theme.accent
    } else {
        theme.text_faint
    };
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .py(px(5.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    svg()
                        .path(tool_icon(&call.name))
                        .size(px(12.0))
                        .text_color(tone),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(if failed { theme.danger } else { theme.text_muted })
                        .child(label),
                )
                .when_some(duration, |element, duration| {
                    element.child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(duration),
                    )
                }),
        )
        .when(!detail.is_empty(), |element| {
            element.child(
                div()
                    .pl(px(18.0))
                    .truncate()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(detail),
            )
        })
        .into_any_element()
}

fn render_markdown_body(
    theme: &Theme,
    body: &str,
    key: u64,
    interaction: &effibuddy_ui::MarkdownInteraction,
    streaming: bool,
) -> AnyElement {
    if streaming {
        effibuddy_ui::render_markdown_streaming_with(theme, body, key, Some(interaction))
    } else {
        effibuddy_ui::render_markdown_keyed_with(theme, body, key, key, Some(interaction))
    }
}

fn render_meta_row(
    assistant: &AssistantItem,
    theme: &Theme,
    streaming: bool,
    turn: Option<&TurnItem>,
    entity: &gpui::WeakEntity<AppView>,
) -> AnyElement {
    let label = if streaming {
        "正在回答…".to_string()
    } else {
        let time = message_time_label(assistant.message.created_at);
        match turn {
            Some(turn) if turn.total_ms > 0 => {
                format!("{time} · {} 步 · {}", turn.members.len(), format_duration_ms(turn.total_ms))
            }
            Some(turn) => format!("{time} · {} 步", turn.members.len()),
            None => time,
        }
    };
    let copy_text = assistant.text.to_string();
    let action_id = assistant.message.id.clone();
    let action_text = assistant.text.to_string();
    let action_entity = entity.clone();
    div()
        .flex()
        .items_center()
        .gap(px(space::SM))
        .child(
            div()
                .id(SharedString::from(format!(
                    "mobile-assistant-actions-{}",
                    assistant.message.id
                )))
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded(px(radius::SM))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    let _ = action_entity.update(cx, |this, cx| {
                        this.open_message_sheet(
                            action_id.clone(),
                            action_text.clone(),
                            false,
                            cx,
                        );
                    });
                })
                .child(
                    svg()
                        .path(icons::MORE)
                        .size(px(12.0))
                        .text_color(theme.text_faint),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("mobile-copy-{}", assistant.message.id)))
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded(px(radius::SM))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    if !copy_text.is_empty() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()));
                    }
                })
                .child(
                    svg()
                        .path(icons::COPY)
                        .size(px(12.0))
                        .text_color(theme.text_faint),
                ),
        )
        .child(
            div()
                .text_size(px(geo::TEXT_META)
                )
                .text_color(theme.text_faint)
                .child(label),
        )
        .into_any_element()
}

fn render_run_status(status: &crate::chat::RunStatusItem, theme: &Theme) -> AnyElement {
    let detail = status
        .detail
        .clone()
        .map(|detail| format!(" · {detail}"))
        .unwrap_or_default();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(space::SM))
        .mb(px(space::LG))
        .child(
            svg()
                .path(icons::LOADER)
                .size(px(13.0))
                .text_color(theme.accent),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(format!("{}{detail}", status.activity)),
        )
        .child(
            div()
                .text_size(px(geo::TEXT_META))
                .text_color(theme.text_faint)
                .child(status.step.to_string()),
        )
        .into_any_element()
}

fn render_notice(theme: &Theme, text: &str, tone: gpui::Hsla, icon: &'static str) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(space::SM))
        .rounded(px(geo::RADIUS_CARD))
        .border_1()
        .border_color(theme.glass_border)
        .bg(theme.surface)
        .px(px(12.0))
        .py(px(10.0))
        .mb(px(space::MD))
        .child(svg().path(icon).size(px(13.0)).text_color(tone))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(12.5))
                .line_height(px(19.0))
                .text_color(tone)
                .child(text.to_string()),
        )
        .into_any_element()
}

fn render_agent_message(theme: &Theme, profile: &str, text: &str) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .rounded(px(geo::RADIUS_CARD))
        .border_1()
        .border_color(theme.glass_border)
        .bg(theme.tertiary_soft)
        .px(px(12.0))
        .py(px(10.0))
        .mb(px(space::MD))
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.tertiary)
                .child(format!("来自 {profile}")),
        )
        .child(
            div()
                .text_size(px(12.5))
                .line_height(px(19.0))
                .text_color(theme.text_muted)
                .child(text.to_string()),
        )
        .into_any_element()
}

