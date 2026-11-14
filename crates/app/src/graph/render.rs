//! GPUI rendering for the relationship canvas.

use effibuddy_agent_core::{AgentNodeKind, AgentVisibility};
use effibuddy_ui::{
    BadgeTone, Button, ButtonSize, ButtonVariant, Spinner, Theme, badge, empty_state, icons,
    shadow_float,
};
use gpui::{
    AnyElement, Context, IntoElement, MouseButton, ScrollDelta, SharedString, Window, div,
    prelude::*, px, svg,
};

use crate::app::AppView;
use crate::chat::{agent_color, format_elapsed};
use crate::graph::canvas::{edge_canvas, grid_canvas};
use crate::graph::model::{GraphFrame, GraphNode, GraphProjection, GraphStatus};

use super::{GraphSelection, ViewMode};

/// Above this many *visible* nodes the graph switches to simplified node
/// strips (no spinner, badges or per-node handlers).
const LOD_NODE_LIMIT: usize = 140;

impl AppView {
    pub(crate) fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        if mode == ViewMode::Chat {
            self.graph.clear_selection();
        } else {
            self.graph.invalidate();
        }
        cx.notify();
    }

    pub(crate) fn persist_graph_viewport(&mut self, cx: &mut gpui::Context<crate::app::AppView>) {
        if self.preview_mode {
            return;
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = effibuddy_agent_core::lock_session(&shared);
            self.graph.save_viewport(&mut session);
        }
        self.persist_active(cx);
    }

    /// Persist the viewport once the gesture settles. Wheel zoom fires many
    /// events per second, and each save serializes the whole session to disk;
    /// debouncing keeps those writes out of the interaction path.
    pub(crate) fn schedule_graph_viewport_save(&mut self, cx: &mut Context<Self>) {
        if self.preview_mode {
            return;
        }
        self.graph_save_generation = self.graph_save_generation.wrapping_add(1);
        let generation = self.graph_save_generation;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.graph_save_generation == generation {
                    this.persist_graph_viewport(cx);
                }
            });
        })
        .detach();
    }

    fn render_graph_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .bottom(px(14.0))
            .left(px(14.0))
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .px(px(8.0))
            .py(px(6.0))
            .shadow(shadow_float(theme))
            .child(
                Button::new("graph-zoom-out", "−", theme)
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.graph.zoom_at(400.0, 300.0, 0.9);
                        this.persist_graph_viewport(cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .min_w(px(40.0))
                    .text_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(format!("{:.0}%", self.graph.zoom * 100.0)),
            )
            .child(
                Button::new("graph-zoom-in", "+", theme)
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.graph.zoom_at(400.0, 300.0, 1.1);
                        this.persist_graph_viewport(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("graph-fit", "适配", theme)
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let [width, height] = this.graph.canvas_size.get();
                        this.graph.fit(width, height);
                        this.persist_graph_viewport(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("graph-reset", "重置", theme)
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.graph.reset_view();
                        this.persist_graph_viewport(cx);
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.0)).h(px(18.0)).bg(theme.border))
            .child(legend_item(
                theme,
                crate::graph::model::GraphEdgeKind::Spawn.label(),
                theme.border_strong,
            ))
            .child(legend_item(
                theme,
                crate::graph::model::GraphEdgeKind::Message.label(),
                theme.agent_blue,
            ))
            .child(legend_item(
                theme,
                crate::graph::model::GraphEdgeKind::Data.label(),
                theme.agent_green,
            ))
            .into_any_element()
    }

    pub(crate) fn render_graph(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let Some(shared) = self.active_shared() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state(
                    &theme,
                    "没有活动会话",
                    "新建一个会话后，智能体关系图会显示在这里。",
                    icons::BRANCH,
                ))
                .into_any_element();
        };
        {
            let session = effibuddy_agent_core::lock_session(&shared);
            self.graph
                .sync(&session, &self.subagent_tasks, self.active_run.is_some());
        }
        // Cloning the `Rc` keeps this O(1); the projection itself is only
        // rebuilt when the graph actually changes.
        let projection = self
            .graph
            .projection
            .clone()
            .unwrap_or_else(|| std::rc::Rc::new(GraphProjection::default()));
        let zoom = self.graph.zoom;
        let pan = self.graph.pan;
        let selected = self.graph.selected.clone();
        let node_count = projection.nodes.len();
        let running = projection
            .nodes
            .iter()
            .filter(|node| node.status.is_running())
            .count();

        let header = div()
            .h(px(52.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(32.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("智能体关系图"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "{} 个节点 · {} 运行中 · {} 条连线",
                                node_count,
                                running,
                                projection.edges.len()
                            )),
                    ),
            )
            .child(div().flex().items_center().gap_2().child(badge(
                &theme,
                "画布可拖动 · 滚轮缩放",
                BadgeTone::Neutral,
            )));

        // Viewport culling: only build elements for what can actually be seen
        // (plus a one-screen margin), so zoomed-in views over large graphs do
        // not pay for invisible nodes.
        let [canvas_w, canvas_h] = self.graph.canvas_size.get();
        let (view_left, view_top, view_right, view_bottom) = {
            let margin = 200.0;
            let left = -pan[0] / zoom - margin;
            let top = -pan[1] / zoom - margin;
            (
                left,
                top,
                left + canvas_w / zoom + margin * 2.0,
                top + canvas_h / zoom + margin * 2.0,
            )
        };
        let visible = |x: f32, y: f32, w: f32, h: f32| {
            x + w >= view_left && x <= view_right && y + h >= view_top && y <= view_bottom
        };

        let frames: Vec<AnyElement> = projection
            .frames
            .iter()
            .filter(|frame| visible(frame.x, frame.y, frame.w, frame.h))
            .map(|frame| {
                render_frame(
                    frame,
                    selected.as_ref() == Some(&GraphSelection::Frame(frame.id.clone())),
                    zoom,
                    pan,
                    &theme,
                    cx,
                )
            })
            .collect();
        // Level of detail: the full node card carries a spinner, badges and
        // interaction handlers (~15 styled elements each). Fit-zoom over a
        // 1000-child graph would rebuild tens of thousands of elements per
        // frame; above the limit, draw a compact strip instead.
        let visible_nodes: Vec<&GraphNode> = projection
            .nodes
            .iter()
            .filter(|node| visible(node.x, node.y, node.w, node.h))
            .collect();
        let simplified = visible_nodes.len() > LOD_NODE_LIMIT;
        let nodes: Vec<AnyElement> = visible_nodes
            .into_iter()
            .map(|node| {
                if simplified {
                    render_node_simplified(node, zoom, pan, &theme)
                } else {
                    render_node(
                        node,
                        selected.as_ref() == Some(&GraphSelection::Node(node.id.clone())),
                        zoom,
                        pan,
                        &theme,
                        cx,
                    )
                }
            })
            .collect();

        let canvas_area = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .min_w(px(0.0))
            .overflow_hidden()
            .bg(theme.bg)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.graph.begin_drag(event.position);
                    this.graph.clear_selection();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.graph.drag.is_some() {
                    this.graph.apply_drag(event.position);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.graph.drag.is_some() {
                        this.graph.end_drag();
                        this.persist_graph_viewport(cx);
                        cx.notify();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                let delta = match event.delta {
                    ScrollDelta::Lines(delta) => delta.y,
                    ScrollDelta::Pixels(delta) => f32::from(delta.y) / 40.0,
                };
                let factor = if delta >= 0.0 { 1.08 } else { 0.92 };
                let position = event.position;
                this.graph
                    .zoom_at(f32::from(position.x), f32::from(position.y), factor);
                this.schedule_graph_viewport_save(cx);
                cx.notify();
            }))
            .child(grid_canvas(pan, zoom, &theme))
            .children(frames)
            .child(edge_canvas(
                &projection,
                pan,
                zoom,
                &theme,
                self.graph.canvas_size.clone(),
            ))
            .children(nodes)
            .child(self.render_graph_toolbar(&theme, cx))
            .when(node_count <= 1, |element| {
                element.child(
                    div()
                        .absolute()
                        .bottom(px(22.0))
                        .right(px(22.0))
                        .max_w(px(420.0))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .p(px(14.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child("还没有其他智能体"),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(17.0))
                                .text_color(theme.text_faint)
                                .child("让主智能体用 `agent` 工具派生子代理，或用 `workflow_run` 定义一个智能体组。节点会实时出现在这里；私有代理只对创建者可见，公开代理可被其他代理复用。"),
                        ),
                )
            });

        let body = div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(canvas_area)
            .when(selected.is_some(), |element| {
                element.child(self.render_graph_inspector(&projection, &theme, cx))
            });

        let _ = window;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

fn legend_item(theme: &Theme, label: &str, color: gpui::Hsla) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(div().size(px(6.0)).rounded(px(999.0)).bg(color))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(label.to_string()),
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

/// Compact node strip for high node counts: 4 styled elements instead of the
/// full card's ~15, and a static status dot instead of an animated spinner.
fn render_node_simplified(node: &GraphNode, zoom: f32, pan: [f32; 2], theme: &Theme) -> AnyElement {
    let color = if node.is_main() {
        theme.text
    } else {
        agent_color(theme, node.color)
    };
    let status_color = match node.status {
        GraphStatus::Running => theme.agent_blue,
        GraphStatus::WaitingApproval => theme.warning,
        GraphStatus::Completed => theme.success,
        GraphStatus::Failed => theme.danger,
        GraphStatus::Cancelled | GraphStatus::Idle => theme.text_faint,
    };
    let font = (11.0 * zoom).clamp(8.0, 13.0);
    let name = truncate(&node.name, (22.0 / zoom.max(0.5)) as usize);

    div()
        .absolute()
        .left(px(node.x * zoom + pan[0]))
        .top(px(node.y * zoom + pan[1]))
        .w(px(node.w * zoom))
        .h(px(node.h * zoom))
        .rounded(px(6.0 * zoom))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(3.0 * zoom.max(0.6)))
                .bg(color),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .justify_center()
                .h_full()
                .pl(px(8.0 * zoom))
                .gap(px(2.0 * zoom))
                .child(
                    div()
                        .text_size(px(font))
                        .text_color(theme.text)
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0 * zoom))
                        .child(
                            div()
                                .size(px(6.0 * zoom.max(0.6)))
                                .rounded(px(999.0))
                                .bg(status_color),
                        )
                        .child(
                            div()
                                .text_size(px((10.0 * zoom).clamp(7.0, 11.0)))
                                .text_color(theme.text_faint)
                                .child(node.status.label().to_string()),
                        ),
                ),
        )
        .into_any_element()
}

fn render_node(
    node: &GraphNode,
    selected: bool,
    zoom: f32,
    pan: [f32; 2],
    theme: &Theme,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let color = if node.is_main() {
        theme.text
    } else {
        agent_color(theme, node.color)
    };
    let soft = {
        let mut value = color;
        value.a = if theme.is_dark() { 0.22 } else { 0.10 };
        value
    };
    let status_color = match node.status {
        GraphStatus::Running => theme.agent_blue,
        GraphStatus::WaitingApproval => theme.warning,
        GraphStatus::Completed => theme.success,
        GraphStatus::Failed => theme.danger,
        GraphStatus::Cancelled | GraphStatus::Idle => theme.text_faint,
    };
    let status_icon: AnyElement = match node.status {
        GraphStatus::Running => Spinner::new(theme.agent_blue, 11.0).into_any_element(),
        GraphStatus::WaitingApproval => svg()
            .path(effibuddy_ui::icons::ALERT)
            .size(px(11.0))
            .text_color(theme.warning)
            .into_any_element(),
        GraphStatus::Completed => svg()
            .path(effibuddy_ui::icons::CHECK)
            .size(px(11.0))
            .text_color(theme.success)
            .into_any_element(),
        GraphStatus::Failed => svg()
            .path(effibuddy_ui::icons::ALERT)
            .size(px(11.0))
            .text_color(theme.danger)
            .into_any_element(),
        _ => div()
            .size(px(6.0))
            .rounded(px(999.0))
            .bg(theme.text_faint)
            .into_any_element(),
    };
    let width = 236.0 * zoom;
    let height = 106.0 * zoom;
    let font = (11.0 * zoom).clamp(8.0, 13.0);
    let title_font = (12.0 * zoom).clamp(9.0, 14.0);
    let id = node.id.clone();
    let open_id = node.id.clone();
    let is_main = node.is_main();
    let visibility = node.visibility;
    let owner = node
        .owner_name
        .clone()
        .filter(|owner| owner != "main")
        .unwrap_or_default();
    let group = node.group_id.clone();
    let name = truncate(&node.name, (18.0 / zoom.max(0.5)) as usize);
    let description = truncate(&node.description, (34.0 / zoom.max(0.5)) as usize);

    div()
        .id(SharedString::from(format!("graph-node-{}", node.id)))
        .absolute()
        .left(px(node.x * zoom + pan[0]))
        .top(px(node.y * zoom + pan[1]))
        .w(px(width))
        .h(px(height))
        // The accent strip below is a square-cornered child. GPUI clips
        // children to a rectangle, not the rounded outline, so keep the left
        // corners square or the strip paints over them.
        .rounded_r(px(11.0 * zoom))
        .border_1()
        .border_color(if selected { theme.text } else { theme.border })
        .bg(theme.surface)
        .when(selected, |element| element.shadow(shadow_float(theme)))
        .when(!selected, |element| {
            element.hover(|style| style.border_color(theme.border_strong))
        })
        .overflow_hidden()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, event, window, cx| {
            cx.stop_propagation();
            this.graph.select_node(id.clone());
            let double = matches!(
                event,
                gpui::ClickEvent::Mouse(mouse) if mouse.down.click_count >= 2
            );
            if double {
                if is_main {
                    this.set_view_mode(ViewMode::Chat, cx);
                    this.composer_focus_handle(cx).focus(window);
                } else {
                    this.set_view_mode(ViewMode::Chat, cx);
                    this.open_agent_tab(&open_id, cx);
                }
            }
            cx.notify();
        }))
        .child(
            div()
                .h_full()
                .flex()
                .child(div().w(px(3.0 * zoom)).h_full().bg(color))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .justify_between()
                        .px(px(9.0 * zoom))
                        .py(px(7.0 * zoom))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.0 * zoom))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(20.0 * zoom))
                                        .rounded(px(6.0 * zoom))
                                        .bg(soft)
                                        .child(
                                            svg()
                                                .path(if is_main {
                                                    effibuddy_ui::icons::SPARKLES
                                                } else if node.kind == AgentNodeKind::Group {
                                                    effibuddy_ui::icons::BRANCH
                                                } else {
                                                    effibuddy_ui::icons::SPARKLES
                                                })
                                                .size(px(11.0 * zoom))
                                                .text_color(color),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(title_font))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(name),
                                )
                                .child(div().flex_none().child(status_icon)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0 * zoom))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(font))
                                        .text_color(theme.text_muted)
                                        .child(description),
                                )
                                .when(!group.is_none(), |element| {
                                    element.child(
                                        div()
                                            .flex_none()
                                            .rounded(px(999.0))
                                            .border_1()
                                            .border_color(theme.border)
                                            .px(px(4.0 * zoom))
                                            .text_size(px((8.5 * zoom).clamp(7.0, 10.0)))
                                            .text_color(theme.text_faint)
                                            .child("组"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.0 * zoom))
                                .text_size(px((9.0 * zoom).clamp(7.0, 11.0)))
                                .text_color(theme.text_faint)
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(status_color)
                                        .child(node.status.label()),
                                )
                                .child("·")
                                .child(format!("{} 工具", node.tool_uses))
                                .when(!owner.is_empty(), |element| {
                                    element.child("·").child(format!("属于 {owner}"))
                                })
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .flex_none()
                                        .rounded(px(999.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .px(px(4.0 * zoom))
                                        .text_color(if visibility == AgentVisibility::Public {
                                            theme.success
                                        } else {
                                            theme.text_faint
                                        })
                                        .child(if visibility == AgentVisibility::Public {
                                            "公开"
                                        } else {
                                            "私有"
                                        }),
                                )
                                .when(node.read_only, |element| {
                                    element.child(
                                        div().flex_none().text_color(theme.warning).child("只读"),
                                    )
                                }),
                        ),
                ),
        )
        .into_any_element()
}

fn render_frame(
    frame: &GraphFrame,
    selected: bool,
    zoom: f32,
    pan: [f32; 2],
    theme: &Theme,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let id = frame.id.clone();
    let status_color = match frame.status {
        GraphStatus::Running => theme.agent_blue,
        GraphStatus::Failed => theme.danger,
        GraphStatus::Cancelled => theme.text_faint,
        _ => theme.success,
    };
    let members = frame.member_ids.len();
    let input_keys = frame
        .input_schema
        .as_ref()
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.as_object())
        .map(|properties| properties.len())
        .unwrap_or(0);
    let output_keys = frame
        .output_schema
        .as_ref()
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.as_object())
        .map(|properties| properties.len())
        .unwrap_or(0);
    div()
        .id(SharedString::from(format!("graph-frame-{}", frame.id)))
        .absolute()
        .left(px(frame.x * zoom + pan[0]))
        .top(px(frame.y * zoom + pan[1]))
        .w(px(frame.w * zoom))
        .h(px(frame.h * zoom))
        .rounded(px(16.0 * zoom))
        .border_1()
        .border_color(if selected {
            theme.text
        } else {
            theme.border_strong
        })
        .bg(theme.surface_alt)
        .cursor_pointer()
        .when(!selected, |element| {
            element.hover(|style| style.border_color(theme.text_muted))
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.graph.select_frame(id.clone());
            cx.notify();
        }))
        .child(
            div()
                .h(px(46.0 * zoom))
                .flex()
                .items_center()
                .gap(px(8.0 * zoom))
                .px(px(14.0 * zoom))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    svg()
                        .path(effibuddy_ui::icons::BRANCH)
                        .size(px(13.0 * zoom))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px((12.0 * zoom).clamp(9.0, 14.0)))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(frame.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px((9.5 * zoom).clamp(7.0, 11.0)))
                        .text_color(status_color)
                        .child(frame.status.label()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px((9.0 * zoom).clamp(7.0, 10.0)))
                        .text_color(theme.text_faint)
                        .child(format!("{members} 成员")),
                )
                .when(input_keys > 0 || output_keys > 0, |element| {
                    element.child(
                        div()
                            .flex_none()
                            .rounded(px(999.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .px(px(6.0 * zoom))
                            .text_size(px((8.5 * zoom).clamp(7.0, 10.0)))
                            .text_color(theme.text_faint)
                            .child(format!("{input_keys} 入 → {output_keys} 出")),
                    )
                }),
        )
        .into_any_element()
}

pub(crate) fn status_dot(theme: &Theme, status: GraphStatus) -> gpui::Hsla {
    match status {
        GraphStatus::Running => theme.agent_blue,
        GraphStatus::WaitingApproval => theme.warning,
        GraphStatus::Completed => theme.success,
        GraphStatus::Failed => theme.danger,
        GraphStatus::Cancelled | GraphStatus::Idle => theme.text_faint,
    }
}

pub(crate) fn elapsed_label(started: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match started {
        Some(started) => format_elapsed(
            chrono::Utc::now()
                .signed_duration_since(started)
                .to_std()
                .unwrap_or_default(),
        ),
        None => "—".to_string(),
    }
}
