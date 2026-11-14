//! Agent relationship graph: view state plus projection, persistence, canvas
//! painting and GPUI rendering.
//!
//! The graph is intentionally split from `app.rs`:
//!
//! - [`model`] computes nodes, group frames, edges and layout;
//! - [`persist`] folds runtime events into the session record;
//! - [`canvas`] paints edges with GPUI's path API;
//! - [`render`] builds the interactive canvas and inspector.
//!
//! `GraphView` only owns viewport, selection and drag state.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use effibuddy_agent_core::SessionState;
use gpui::{Pixels, Point};

use crate::app::AgentTask;

use model::GraphProjection;

pub mod canvas;
pub mod inspector;
pub mod model;
pub mod persist;
pub mod render;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Chat,
    Graph,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSelection {
    Node(String),
    Frame(String),
}

pub struct GraphView {
    pub pan: [f32; 2],
    pub zoom: f32,
    pub selected: Option<GraphSelection>,
    /// Mouse-down position and pan at drag start.
    pub drag: Option<(Point<Pixels>, [f32; 2])>,
    /// Shared projection: `render_graph` clones the `Rc`, never the graph.
    pub projection: Option<Rc<GraphProjection>>,
    pub dirty: bool,
    /// Last measured canvas size, written by the edge canvas during paint so
    /// "fit" can use the real viewport instead of a guessed one.
    pub canvas_size: Rc<Cell<[f32; 2]>>,
    /// Pretty-printed inspector JSON, keyed by `selection:field#max_chars`.
    /// Cleared whenever the projection is rebuilt, so entries never go stale.
    json_cache: RefCell<HashMap<String, Rc<str>>>,
    session_id: Option<String>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphView {
    pub fn new() -> Self {
        Self {
            pan: [0.0, 0.0],
            zoom: 1.0,
            selected: None,
            drag: None,
            projection: None,
            dirty: true,
            canvas_size: Rc::new(Cell::new([1100.0, 680.0])),
            json_cache: RefCell::new(HashMap::new()),
            session_id: None,
        }
    }

    /// Rebuild the projection when needed. Switching sessions restores the
    /// persisted viewport and clears selection.
    pub fn sync(&mut self, session: &SessionState, tasks: &[AgentTask], main_running: bool) {
        if self.session_id.as_deref() != Some(session.id.as_str()) {
            // Only a real session switch clears selection; the first bind must
            // keep a selection primed by the caller (e.g. the preview fixture).
            let switching = self.session_id.is_some();
            self.session_id = Some(session.id.clone());
            self.pan = [session.graph.pan.x, session.graph.pan.y];
            self.zoom = session.graph.zoom.clamp(0.35, 2.2);
            if switching {
                self.selected = None;
            }
            self.dirty = true;
        }
        if self.dirty || self.projection.is_none() {
            self.projection = Some(Rc::new(model::build_projection(
                session,
                tasks,
                main_running,
            )));
            self.json_cache.borrow_mut().clear();
            self.dirty = false;
        }
    }

    /// Cached pretty-printed JSON for the inspector. Rebuilding the projection
    /// clears the cache, so a hit always matches the current graph.
    pub fn json_text(&self, key: &str, value: &serde_json::Value, max_chars: usize) -> Rc<str> {
        let cache_key = format!("{key}#{max_chars}");
        if let Some(text) = self.json_cache.borrow().get(&cache_key) {
            return text.clone();
        }
        let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        let text: Rc<str> = truncate_chars(&pretty, max_chars).into();
        self.json_cache.borrow_mut().insert(cache_key, text.clone());
        text
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn select_node(&mut self, id: impl Into<String>) {
        self.selected = Some(GraphSelection::Node(id.into()));
    }

    pub fn select_frame(&mut self, id: impl Into<String>) {
        self.selected = Some(GraphSelection::Frame(id.into()));
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Zoom around a screen-space cursor position so the world point under the
    /// pointer stays put.
    pub fn zoom_at(&mut self, cursor_x: f32, cursor_y: f32, factor: f32) {
        let old = self.zoom;
        let next = (old * factor).clamp(0.35, 2.2);
        if (next - old).abs() < f32::EPSILON {
            return;
        }
        let world_x = (cursor_x - self.pan[0]) / old;
        let world_y = (cursor_y - self.pan[1]) / old;
        self.zoom = next;
        self.pan[0] = cursor_x - world_x * next;
        self.pan[1] = cursor_y - world_y * next;
    }

    pub fn reset_view(&mut self) {
        self.pan = [32.0, 32.0];
        self.zoom = 1.0;
    }

    /// Fit the whole graph into a viewport of `width x height`.
    pub fn fit(&mut self, width: f32, height: f32) {
        let Some(projection) = &self.projection else {
            self.reset_view();
            return;
        };
        let (content_w, content_h) = projection.bounds;
        if content_w <= 1.0 || content_h <= 1.0 {
            self.reset_view();
            return;
        }
        let zoom = ((width - 64.0) / content_w)
            .min((height - 64.0) / content_h)
            .clamp(0.35, 1.6);
        self.zoom = zoom;
        self.pan = [
            (width - content_w * zoom).max(24.0) * 0.5,
            (height - content_h * zoom).max(24.0) * 0.5,
        ];
    }

    pub fn save_viewport(&self, session: &mut SessionState) {
        persist::save_viewport(&mut session.graph, self.pan, self.zoom);
    }

    pub fn apply_drag(&mut self, current: Point<Pixels>) {
        let Some((start, pan)) = &self.drag else {
            return;
        };
        self.pan = [
            pan[0] + f32::from(current.x - start.x),
            pan[1] + f32::from(current.y - start.y),
        ];
    }

    pub fn begin_drag(&mut self, position: Point<Pixels>) {
        self.drag = Some((position, self.pan));
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}
