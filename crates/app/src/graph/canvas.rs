//! Canvas painting for graph edges and the dotted background.
//!
//! Nodes and group frames are normal GPUI elements layered above this canvas;
//! only the connectors and grid are painted at the low level.

use effibuddy_ui::Theme;
use gpui::{
    AnyElement, Bounds, PathBuilder, Pixels, Window, canvas, fill, point, prelude::*, px, size,
};

use super::model::{GraphEdgeKind, GraphProjection};

const EDGE_WIDTH: f32 = 1.6;
const ARROW: f32 = 7.0;
const GRID_STEP: f32 = 40.0;

struct EdgeDraw {
    from: (f32, f32),
    to: (f32, f32),
    kind: GraphEdgeKind,
    emphasis: bool,
}

/// Dot grid, painted below group frames so frame surfaces stay clean.
pub fn grid_canvas(pan: [f32; 2], zoom: f32, theme: &Theme) -> AnyElement {
    let grid_color = theme.border;
    canvas(
        move |bounds, _window, _cx| bounds,
        move |bounds: Bounds<Pixels>, _state, window: &mut Window, _cx| {
            paint_grid(bounds, pan, zoom, grid_color, window);
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// Edge connectors, painted above group frames (so links between members are
/// visible) but below agent nodes. Also reports the measured canvas size back
/// to the view so "fit" can use the real viewport.
pub fn edge_canvas(
    projection: &GraphProjection,
    pan: [f32; 2],
    zoom: f32,
    theme: &Theme,
    size_cell: std::rc::Rc<std::cell::Cell<[f32; 2]>>,
) -> AnyElement {
    let edges = collect_edges(projection);
    let spawn_color = theme.border_strong;
    let message_color = theme.agent_blue;
    let data_color = theme.agent_green;
    canvas(
        move |bounds, _window, _cx| {
            size_cell.set([f32::from(bounds.size.width), f32::from(bounds.size.height)]);
            bounds
        },
        move |bounds: Bounds<Pixels>, _state, window: &mut Window, _cx| {
            // Edges are computed in canvas-local coordinates; shift them by the
            // canvas origin so they line up with the absolutely positioned
            // nodes and frames.
            let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            const CULL_MARGIN: f32 = 80.0;
            for edge in &edges {
                let source = to_screen(edge.from, origin, pan, zoom);
                let target = to_screen(edge.to, origin, pan, zoom);
                // Skip edges whose bounding box is entirely outside the canvas;
                // with a large graph most edges are off-screen while zoomed in.
                let (min_x, max_x) = (source.0.min(target.0), source.0.max(target.0));
                let (min_y, max_y) = (source.1.min(target.1), source.1.max(target.1));
                if max_x < origin.0 - CULL_MARGIN
                    || min_x > origin.0 + width + CULL_MARGIN
                    || max_y < origin.1 - CULL_MARGIN
                    || min_y > origin.1 + height + CULL_MARGIN
                {
                    continue;
                }
                let (color, width) = match edge.kind {
                    GraphEdgeKind::Spawn => (spawn_color, EDGE_WIDTH),
                    GraphEdgeKind::Message => {
                        (message_color, if edge.emphasis { 2.4 } else { EDGE_WIDTH })
                    }
                    GraphEdgeKind::Data => (data_color, EDGE_WIDTH),
                };
                paint_edge(source, target, color, width, window);
            }
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

fn collect_edges(projection: &GraphProjection) -> Vec<EdgeDraw> {
    let mut out = Vec::new();
    for edge in &projection.edges {
        let Some(from) = projection.rect(&edge.from) else {
            continue;
        };
        let Some(to) = projection.rect(&edge.to) else {
            continue;
        };
        let from_x = from.0 + from.2;
        let from_y = from.1 + from.3 * 0.5;
        let to_x = to.0;
        let to_y = to.1 + to.3 * 0.5;
        // Back-edges connect left-to-right so the curve does not cross the
        // node body.
        let (from_x, from_y, to_x, to_y) = if to_x >= from_x {
            (from_x, from_y, to_x, to_y)
        } else {
            (from.0, from_y, to.0 + to.2, to_y)
        };
        out.push(EdgeDraw {
            from: (from_x, from_y),
            to: (to_x, to_y),
            kind: edge.kind,
            emphasis: edge.count > 1,
        });
    }
    out
}

fn to_screen(world: (f32, f32), origin: (f32, f32), pan: [f32; 2], zoom: f32) -> (f32, f32) {
    (
        origin.0 + world.0 * zoom + pan[0],
        origin.1 + world.1 * zoom + pan[1],
    )
}

fn paint_edge(
    from: (f32, f32),
    to: (f32, f32),
    color: gpui::Hsla,
    width: f32,
    window: &mut Window,
) {
    let dx = (to.0 - from.0).abs().max(40.0) * 0.5;
    let control_a = (from.0 + dx, from.1);
    let control_b = (to.0 - dx, to.1);
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(point(px(from.0), px(from.1)));
    builder.cubic_bezier_to(
        point(px(to.0), px(to.1)),
        point(px(control_a.0), px(control_a.1)),
        point(px(control_b.0), px(control_b.1)),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
    // Arrowhead pointing toward the target. The tangent at the endpoint is
    // horizontal for the bezier used above.
    let direction = if to.0 >= from.0 { 1.0 } else { -1.0 };
    let mut arrow = PathBuilder::fill();
    arrow.move_to(point(px(to.0), px(to.1)));
    arrow.line_to(point(px(to.0 - direction * ARROW), px(to.1 - ARROW * 0.5)));
    arrow.line_to(point(px(to.0 - direction * ARROW), px(to.1 + ARROW * 0.5)));
    arrow.close();
    if let Ok(path) = arrow.build() {
        window.paint_path(path, color);
    }
}

fn paint_grid(
    bounds: Bounds<Pixels>,
    pan: [f32; 2],
    zoom: f32,
    color: gpui::Hsla,
    window: &mut Window,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let mut step = GRID_STEP * zoom;
    // Dots closer than ~20px read as noise and multiply primitives.
    if step < 20.0 {
        return;
    }
    // Keep the per-frame paint primitive count bounded on very large canvases.
    const MAX_DOTS: f32 = 2200.0;
    while (width / step) * (height / step) > MAX_DOTS {
        step *= 1.5;
    }
    let start_x = pan[0].rem_euclid(step);
    let start_y = pan[1].rem_euclid(step);
    let mut x = start_x;
    while x < width {
        let mut y = start_y;
        while y < height {
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        px(f32::from(bounds.origin.x) + x),
                        px(f32::from(bounds.origin.y) + y),
                    ),
                    size(px(1.4), px(1.4)),
                ),
                color.opacity(0.55),
            ));
            y += step;
        }
        x += step;
    }
}
