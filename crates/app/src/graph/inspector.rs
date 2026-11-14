//! Right-hand inspector for a selected node or group frame.
//!
//! The inspector is intentionally a read-only projection: it shows what the
//! agent was asked, what it returned, its contract and its network identity.
//! Double-clicking a node (or using the button here) opens the full transcript
//! in the side pane, which reuses the main chat renderer.

use effibuddy_agent_core::{AgentNodeKind, AgentVisibility};
use effibuddy_ui::{
    BadgeTone, Button, ButtonSize, ButtonVariant, IconButton, Theme, badge, divider, icons,
    section_label,
};
use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};

use crate::app::AppView;
use crate::chat::agent_color;
use crate::graph::model::{GraphFrame, GraphNode, GraphProjection, GraphStatus};
use crate::graph::render::{elapsed_label, status_dot};

use super::GraphSelection;

impl AppView {
    pub(crate) fn render_graph_inspector(
        &self,
        projection: &GraphProjection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(selection) = self.graph.selected.clone() else {
            return div().into_any_element();
        };
        let content = match &selection {
            GraphSelection::Node(id) => projection
                .node(id)
                .map(|node| self.inspector_node(projection, node, theme, cx)),
            GraphSelection::Frame(id) => projection
                .frame(id)
                .map(|frame| self.inspector_frame(frame, projection, theme, cx)),
        };
        let Some(content) = content else {
            return div().into_any_element();
        };
        div()
            .w(px(348.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                div()
                    .h(px(44.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(section_label(theme, "节点详情"))
                    .child(
                        IconButton::new("graph-inspector-close", icons::X, theme)
                            .size(24.0, 12.0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.graph.clear_selection();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("graph-inspector-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .px_4()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .child(content),
            )
            .into_any_element()
    }

    fn inspector_node(
        &self,
        projection: &GraphProjection,
        node: &GraphNode,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = if node.is_main() {
            theme.text
        } else {
            agent_color(theme, node.color)
        };
        let status_color = status_dot(theme, node.status);
        let live_task = node
            .agent_id
            .as_deref()
            .and_then(|id| self.subagent_tasks.iter().find(|task| task.agent_id == id));
        let can_open = live_task.is_some();
        let open_id = node.agent_id.clone().unwrap_or_else(|| node.id.clone());
        let is_main = node.is_main();

        let mut column = div().flex().flex_col().gap(px(14.0));
        column = column.child(
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
                        .size(px(30.0))
                        .rounded(px(8.0))
                        .bg(color)
                        .child(
                            gpui::svg()
                                .path(icons::SPARKLES)
                                .size(px(14.0))
                                .text_color(theme.on_accent),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(node.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(11.0))
                                .child(div().text_color(status_color).child(node.status.label()))
                                .child(div().text_color(theme.text_faint).child("·"))
                                .child(div().text_color(theme.text_faint).child(node.kind.label())),
                        ),
                ),
        );
        column = column.child(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .child(badge(
                    theme,
                    if node.visibility == AgentVisibility::Public {
                        "公开"
                    } else {
                        "私有"
                    },
                    BadgeTone::Neutral,
                ))
                .when(node.read_only, |element| {
                    element.child(badge(theme, "只读", BadgeTone::Warning))
                })
                .when(node.can_spawn, |element| {
                    element.child(badge(theme, "可派生", BadgeTone::Neutral))
                })
                .when(node.live, |element| {
                    element.child(badge(theme, "实时", BadgeTone::Accent))
                }),
        );
        if !node.description.trim().is_empty() {
            column = column.child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(18.0))
                    .text_color(theme.text_muted)
                    .child(node.description.clone()),
            );
        }
        let owner = node
            .owner_name
            .clone()
            .or_else(|| node.parent_id.clone())
            .unwrap_or_else(|| "main".to_string());
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(metric_row(theme, "所属", &owner))
                .child(metric_row(theme, "Profile", &node.profile))
                .child(metric_row(theme, "层级", &node.depth.to_string()))
                .child(metric_row(theme, "工具调用", &node.tool_uses.to_string()))
                .child(metric_row(theme, "Tokens", &node.total_tokens.to_string()))
                .child(metric_row(
                    theme,
                    "耗时",
                    &if node.duration_ms > 0 {
                        format!("{} ms", node.duration_ms)
                    } else {
                        elapsed_label(node.started_at)
                    },
                ))
                .when_some(node.session_id.clone(), |element, session| {
                    element.child(metric_row(theme, "子会话", &short_id(&session)))
                }),
        );
        column = column.child(divider(theme));
        column = column.child(section_label(theme, "输入"));
        column = column.child(text_card(
            theme,
            node.task.as_deref().unwrap_or("(没有记录到任务输入)"),
            1600,
        ));
        column = column.child(section_label(theme, "输出"));
        column = column.child(text_card(
            theme,
            node.output.as_deref().unwrap_or("(还没有输出)"),
            3000,
        ));
        if let Some(data) = &node.data {
            column = column.child(json_card(
                theme,
                "结构化数据",
                &self
                    .graph
                    .json_text(&format!("node:{}:data", node.id), data, 3000),
            ));
        }
        if let Some(schema) = &node.output_schema {
            column = column.child(json_card(
                theme,
                "输出契约",
                &self
                    .graph
                    .json_text(&format!("node:{}:output_schema", node.id), schema, 2000),
            ));
        }
        if let Some(schema) = &node.input_schema {
            column = column.child(json_card(
                theme,
                "输入契约",
                &self
                    .graph
                    .json_text(&format!("node:{}:input_schema", node.id), schema, 1600),
            ));
        }
        let relations: Vec<&crate::graph::model::GraphEdge> = projection
            .edges
            .iter()
            .filter(|edge| edge.from == node.id || edge.to == node.id)
            .collect();
        if !relations.is_empty() {
            column = column.child(divider(theme));
            column = column.child(section_label(theme, "关系"));
            for edge in relations {
                let outgoing = edge.from == node.id;
                let other_id = if outgoing { &edge.to } else { &edge.from };
                let other = projection
                    .node(other_id)
                    .map(|node| node.name.clone())
                    .or_else(|| projection.frame(other_id).map(|frame| frame.name.clone()))
                    .unwrap_or_else(|| other_id.clone());
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(if outgoing { "→" } else { "←" })
                        .child(div().flex_none().text_color(theme.text_muted).child(other))
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.text_faint)
                                .child(edge.kind.label()),
                        )
                        .child(div().min_w(px(0.0)).truncate().child(edge.label.clone())),
                );
            }
        }
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .when(is_main, |element| {
                    element.child(
                        Button::new("inspector-main-chat", "返回主会话", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_view_mode(super::ViewMode::Chat, cx);
                                this.composer_focus_handle(cx).focus(window);
                            })),
                    )
                })
                .when(!is_main && can_open, |element| {
                    element.child(
                        Button::new("inspector-open-agent", "打开完整会话", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_agent_tab(&open_id, cx);
                                this.set_view_mode(super::ViewMode::Chat, cx);
                                this.graph.clear_selection();
                                cx.notify();
                            })),
                    )
                })
                .when(!is_main && !can_open, |element| {
                    element.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child("历史记录 · 无活动子会话"),
                    )
                }),
        );
        column.into_any_element()
    }

    fn inspector_frame(
        &self,
        frame: &GraphFrame,
        projection: &GraphProjection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status_color = status_dot(theme, frame.status);
        let members: Vec<&GraphNode> = frame
            .member_ids
            .iter()
            .filter_map(|id| projection.node(id))
            .collect();
        let mut column = div().flex().flex_col().gap(px(14.0));
        column = column.child(
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
                        .size(px(30.0))
                        .rounded(px(8.0))
                        .bg(theme.surface_alt)
                        .child(
                            gpui::svg()
                                .path(icons::BRANCH)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(frame.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(11.0))
                                .child(div().text_color(status_color).child(frame.status.label()))
                                .child(div().text_color(theme.text_faint).child("·"))
                                .child(
                                    div()
                                        .text_color(theme.text_faint)
                                        .child(format!("{} 个成员", members.len())),
                                ),
                        ),
                ),
        );
        if !frame.description.trim().is_empty() {
            column = column.child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(18.0))
                    .text_color(theme.text_muted)
                    .child(frame.description.clone()),
            );
        }
        if let Some(error) = &frame.error {
            column = column.child(
                div()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.danger)
                    .bg(theme.danger_soft)
                    .px(px(9.0))
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        column = column.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(format!("耗时 {}", elapsed_label(frame.started_at)))
                .child(format!("{} 个成员", members.len())),
        );
        column = column.child(section_label(theme, "成员"));
        for member in &members {
            let id = member.id.clone();
            let color = agent_color(theme, member.color);
            column = column.child(
                div()
                    .id(SharedString::from(format!("frame-member-{}", member.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .px(px(8.0))
                    .py(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_alt))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.graph.select_node(id.clone());
                        cx.notify();
                    }))
                    .child(div().size(px(7.0)).rounded(px(999.0)).bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(theme.text)
                                    .child(member.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_faint)
                                    .child(format!(
                                        "{} · {} · {}",
                                        member.profile,
                                        member.status.label(),
                                        member.visibility.label()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(format!("{} 工具", member.tool_uses)),
                    ),
            );
        }
        column = column.child(divider(theme));
        column = column.child(section_label(theme, "组输入"));
        column = column.child(match &frame.input {
            Some(input) => json_card(
                theme,
                "",
                &self
                    .graph
                    .json_text(&format!("frame:{}:input", frame.id), input, 2000),
            ),
            None => text_card(theme, "(没有记录到组输入)", 400),
        });
        column = column.child(section_label(theme, "组输出"));
        column = column.child(match &frame.output {
            Some(output) => json_card(
                theme,
                "",
                &self
                    .graph
                    .json_text(&format!("frame:{}:output", frame.id), output, 3000),
            ),
            None => text_card(theme, "(还没有组输出)", 400),
        });
        if let Some(data) = &frame.data {
            column = column.child(json_card(
                theme,
                "结构化数据",
                &self
                    .graph
                    .json_text(&format!("frame:{}:data", frame.id), data, 2400),
            ));
        }
        if let Some(schema) = &frame.input_schema {
            column = column.child(json_card(
                theme,
                "输入契约",
                &self
                    .graph
                    .json_text(&format!("frame:{}:input_schema", frame.id), schema, 1600),
            ));
        }
        if let Some(schema) = &frame.output_schema {
            column = column.child(json_card(
                theme,
                "输出契约",
                &self
                    .graph
                    .json_text(&format!("frame:{}:output_schema", frame.id), schema, 1600),
            ));
        }
        let _ = cx;
        column.into_any_element()
    }
}

fn metric_row(theme: &Theme, label: &str, value: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(value.to_string()),
        )
        .into_any_element()
}

fn text_card(theme: &Theme, text: &str, max_chars: usize) -> AnyElement {
    div()
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .px(px(10.0))
        .py(px(8.0))
        .text_size(px(11.0))
        .line_height(px(17.0))
        .text_color(theme.text_muted)
        .child(truncate(text, max_chars))
        .into_any_element()
}

fn json_card(theme: &Theme, title: &str, text: &str) -> AnyElement {
    let mut card = div()
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.code_bg)
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_1();
    if !title.is_empty() {
        card = card.child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(title.to_string()),
        );
    }
    card.child(
        div()
            .font_family(theme.mono_font())
            .text_size(px(10.0))
            .line_height(px(15.0))
            .text_color(theme.text_muted)
            .child(text.to_string()),
    )
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

fn short_id(id: &str) -> String {
    format!("{}…", id.chars().take(8).collect::<String>())
}

#[allow(dead_code)]
fn kind_badge(theme: &Theme, kind: AgentNodeKind) -> AnyElement {
    badge(theme, kind.label(), BadgeTone::Neutral).into_any_element()
}

#[allow(dead_code)]
fn status_label(status: GraphStatus) -> &'static str {
    status.label()
}
