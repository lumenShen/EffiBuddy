//! Keyed layout motion for GPUI element trees.
//!
//! GPUI re-runs layout when state changes and does not keep a mutable DOM
//! around, so there is no `getBoundingClientRect()` to read before a mutation.
//! This module keeps the last painted bounds per stable [`MotionId`] and turns
//! that snapshot into a paint-time offset, the same idea as the web's FLIP
//! technique.
//!
//! A [`MotionItem`] wraps one keyed child. When a transition is active it
//! shifts the child's prepaint origin without touching its layout or size.
//! That makes reorder, reparent and "appears at the old place, settles at the
//! new one" cheap: no taffy re-layout, no text reshaping, just new primitive
//! origins in the scene.
//!
//! Only position is animated here. Size changes still reflow the subtree (they
//! snap rather than tween), and opacity is app-owned (GPUI applies opacity per
//! primitive, not per group). Use [`MotionController::progress`] /
//! [`MotionController::enter_progress`] to drive those effects from
//! application state.
//!
//! Cross-parent moves should enable [`MotionItem::overlay_while_animating`]:
//! the item is laid out by its destination but painted above both parents for
//! the duration of the flight, so the destination's `overflow_hidden` cannot
//! clip it mid-way.
//!
//! Exit animations need the application to keep the leaving item alive for one
//! transition: GPUI drops elements as soon as they leave the tree. The store
//! exposes [`MotionController::bounds`] so a ghost can be painted at the last
//! seen position while it fades out.
//!
//! ```ignore
//! // before mutating the data that drives the list
//! self.motion.begin(MotionSpec::LAYOUT);
//! self.items.reverse();
//! cx.notify();
//!
//! // in render
//! div().flex().flex_col().children(items.iter().enumerate().map(|(index, item)| {
//!     motion_item(&self.motion, item.id.clone(), render_item(item))
//!         .delay(Duration::from_millis(index as u64 * 15))
//!         .into_any_element()
//! }))
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, SharedString, Window, point,
};

/// Stable identity for a motion item.
///
/// Use the application's data id, not GPUI's `ElementId`: the latter encodes
/// the parent path and therefore changes when an item is reparented.
pub type MotionId = SharedString;

/// Small controls (switch, disclosure chevron) settle quickly.
pub const CONTROL_MOTION: Duration = Duration::from_millis(150);
/// Switch state changes get a hair more time than a chevron.
pub const SWITCH_MOTION: Duration = Duration::from_millis(160);
/// Mouse-triggered overlays (menus, dialogs) share the approval-card beat.
pub const OVERLAY_MOTION: Duration = Duration::from_millis(180);

/// The app's ease-out cubic, used by chat entrances and approval cards.
pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Entrance duration for overlays. Reduced motion collapses to a single frame
/// instead of zero, which would make `AnimationElement` divide by zero.
pub fn overlay_motion_duration() -> Duration {
    if reduce_motion() {
        Duration::from_nanos(1)
    } else {
        OVERLAY_MOTION
    }
}

/// Opt-out for users who ask the OS/app for reduced motion.
///
/// GPUI 0.2.2 does not expose the platform accessibility setting, so this is
/// an app-level switch: `EFFIBUDDY_REDUCE_MOTION=1` (or any value other than
/// `0`) disables the structural animations added here. Existing continuous
/// indicators (spinner/pulse) are intentionally left running: they communicate
/// loading state rather than decorating a transition.
pub fn reduce_motion() -> bool {
    static REDUCE: OnceLock<bool> = OnceLock::new();
    *REDUCE.get_or_init(|| {
        std::env::var("EFFIBUDDY_REDUCE_MOTION")
            .ok()
            .is_some_and(|value| value != "0")
    })
}

/// Blend two colors in HSLA space. Used by the switch track/knob; the app's
/// palette is low-chroma, so per-channel interpolation stays clean.
pub fn mix_color(from: gpui::Hsla, to: gpui::Hsla, t: f32) -> gpui::Hsla {
    let t = t.clamp(0.0, 1.0);
    gpui::Hsla {
        h: from.h + (to.h - from.h) * t,
        s: from.s + (to.s - from.s) * t,
        l: from.l + (to.l - from.l) * t,
        a: from.a + (to.a - from.a) * t,
    }
}

/// A cubic Bézier easing curve with control points `(x1, y1)` and `(x2, y2)`.
///
/// This follows the CSS `cubic-bezier()` convention: `x1` and `x2` should be
/// in `0..=1`. The curve is solved for `x`, so `ease(0) == 0` and
/// `ease(1) == 1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Evaluate `y` at the curve parameter selected by `x = t`.
    pub fn ease(self, t: f32) -> f32 {
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        let u = self.solve_for_x(t);
        sample_curve(u, self.y1, self.y2)
    }

    fn solve_for_x(self, x: f32) -> f32 {
        let (a, b, c) = coefficients(self.x1, self.x2);
        let sample = |t: f32| ((a * t + b) * t + c) * t;

        // Newton-Raphson converges quickly for the well-behaved curves used by
        // UI easing. Fall back to bisection if the slope collapses.
        let mut t = x;
        for _ in 0..8 {
            let error = sample(t) - x;
            if error.abs() < 1e-6 {
                return t;
            }
            let slope = (3.0 * a * t + 2.0 * b) * t + c;
            if slope.abs() < 1e-4 {
                break;
            }
            let next = t - error / slope;
            if !(0.0..=1.0).contains(&next) {
                break;
            }
            t = next;
        }

        let mut low = 0.0;
        let mut high = 1.0;
        let mut t = x.clamp(0.0, 1.0);
        for _ in 0..20 {
            let value = sample(t);
            if (value - x).abs() < 1e-6 {
                return t;
            }
            if value < x {
                low = t;
            } else {
                high = t;
            }
            t = (low + high) * 0.5;
        }
        t
    }
}

fn coefficients(control1: f32, control2: f32) -> (f32, f32, f32) {
    (
        1.0 - 3.0 * control2 + 3.0 * control1,
        3.0 * control2 - 6.0 * control1,
        3.0 * control1,
    )
}

fn sample_curve(t: f32, control1: f32, control2: f32) -> f32 {
    let (a, b, c) = coefficients(control1, control2);
    ((a * t + b) * t + c) * t
}

/// Named easing curves plus a custom cubic-Bézier escape hatch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Easing {
    Linear,
    /// Decelerating settle, close to CSS `ease-out` with a longer tail.
    #[default]
    EaseOut,
    /// Accelerate then decelerate.
    EaseInOut,
    CubicBezier(CubicBezier),
}

impl Easing {
    pub fn ease(self, t: f32) -> f32 {
        match self {
            Easing::Linear => t.clamp(0.0, 1.0),
            Easing::EaseOut => CubicBezier::new(0.16, 1.0, 0.3, 1.0).ease(t),
            Easing::EaseInOut => CubicBezier::new(0.65, 0.0, 0.35, 1.0).ease(t),
            Easing::CubicBezier(curve) => curve.ease(t),
        }
    }
}

/// Timing for a layout transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionSpec {
    pub duration: Duration,
    pub easing: Easing,
}

impl MotionSpec {
    /// Default for reorder / reparent transitions: short enough to feel
    /// immediate, long enough for the eye to follow the item.
    pub const LAYOUT: Self = Self {
        duration: Duration::from_millis(220),
        easing: Easing::EaseOut,
    };

    pub const fn new(duration: Duration, easing: Easing) -> Self {
        Self { duration, easing }
    }
}

impl Default for MotionSpec {
    fn default() -> Self {
        Self::LAYOUT
    }
}

/// Per-item result for the current frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionFrame {
    /// Offset to apply to the child's paint origin. Layout is untouched.
    pub offset: Point<Pixels>,
    /// Eased progress in `0..=1`, suitable for app-driven opacity effects.
    pub progress: f32,
    /// Whether this item still needs animation frames.
    pub animating: bool,
}

/// Shared transition state. Clone the controller into each [`MotionItem`];
/// it is UI-thread local, like `ScrollHandle`.
#[derive(Clone)]
pub struct MotionController {
    inner: Rc<RefCell<MotionStore>>,
}

impl MotionController {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MotionStore::default())),
        }
    }

    /// Snapshot the last painted bounds and start a new transition.
    ///
    /// Call this immediately *before* mutating the state that drives the
    /// element tree. Returns `false` when nothing has been painted yet, so an
    /// initial render never animates from an empty snapshot.
    pub fn begin(&self, spec: MotionSpec) -> bool {
        if reduce_motion() {
            return false;
        }
        self.inner.borrow_mut().begin_at(Instant::now(), spec)
    }

    /// Eased progress of the current transition, accounting for a per-item
    /// delay. Returns `1.0` when no transition is active.
    pub fn progress(&self, delay: Duration) -> f32 {
        self.inner.borrow().progress_at(Instant::now(), delay)
    }

    /// Progress for an item that was absent when the current transition
    /// began, or `None` if the item was already present.
    ///
    /// Use this for enter opacity:
    /// `if let Some(p) = motion.enter_progress(&id) { row = row.opacity(p); }`
    pub fn enter_progress(&self, id: &MotionId) -> Option<f32> {
        self.enter_progress_with_delay(id, Duration::ZERO)
    }

    /// [`Self::enter_progress`] with the same per-item delay passed to
    /// [`MotionItem::delay`], for staggered enters.
    pub fn enter_progress_with_delay(&self, id: &MotionId, delay: Duration) -> Option<f32> {
        let store = self.inner.borrow();
        store.started_at?;
        if store.old_bounds.contains_key(id) {
            return None;
        }
        Some(store.progress_at(Instant::now(), delay))
    }

    /// Last painted bounds for an id. Useful for painting an exit ghost after
    /// the real element has left the tree.
    pub fn bounds(&self, id: &MotionId) -> Option<Bounds<Pixels>> {
        self.inner.borrow().visual_bounds.get(id).cloned()
    }

    /// Drop remembered bounds for an id that will not return (for example an
    /// item that was deleted rather than reordered).
    pub fn forget(&self, id: &MotionId) {
        let mut store = self.inner.borrow_mut();
        store.visual_bounds.remove(id);
        store.old_bounds.remove(id);
    }

    fn frame(
        &self,
        id: &MotionId,
        layout: Bounds<Pixels>,
        now: Instant,
        delay: Duration,
        enter_offset: Point<Pixels>,
    ) -> MotionFrame {
        self.inner
            .borrow_mut()
            .frame_at(now, id, layout, delay, enter_offset)
    }
}

impl Default for MotionController {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct MotionStore {
    spec: MotionSpec,
    started_at: Option<Instant>,
    /// Bounds captured at `begin`, used as the "from" side of the transition.
    old_bounds: HashMap<MotionId, Bounds<Pixels>>,
    /// Last painted bounds, updated every frame.
    visual_bounds: HashMap<MotionId, Bounds<Pixels>>,
}

impl MotionStore {
    fn begin_at(&mut self, now: Instant, spec: MotionSpec) -> bool {
        if self.visual_bounds.is_empty() {
            self.started_at = None;
            self.old_bounds.clear();
            return false;
        }
        self.old_bounds = self.visual_bounds.clone();
        self.spec = spec;
        self.started_at = Some(now);
        true
    }

    fn progress_at(&self, now: Instant, delay: Duration) -> f32 {
        let Some(started_at) = self.started_at else {
            return 1.0;
        };
        let elapsed = now.saturating_duration_since(started_at);
        let local = elapsed.saturating_sub(delay);
        let duration = self.spec.duration.as_secs_f32();
        let linear = if duration <= f32::EPSILON {
            1.0
        } else {
            (local.as_secs_f32() / duration).clamp(0.0, 1.0)
        };
        self.spec.easing.ease(linear)
    }

    fn frame_at(
        &mut self,
        now: Instant,
        id: &MotionId,
        layout: Bounds<Pixels>,
        delay: Duration,
        enter_offset: Point<Pixels>,
    ) -> MotionFrame {
        let Some(started_at) = self.started_at else {
            self.visual_bounds.insert(id.clone(), layout);
            return MotionFrame {
                offset: Point::default(),
                progress: 1.0,
                animating: false,
            };
        };

        let elapsed = now.saturating_duration_since(started_at);
        let local = elapsed.saturating_sub(delay);
        let duration = self.spec.duration.as_secs_f32();
        let linear = if duration <= f32::EPSILON {
            1.0
        } else {
            (local.as_secs_f32() / duration).clamp(0.0, 1.0)
        };
        let progress = self.spec.easing.ease(linear);

        // An item that existed when the transition began starts from its last
        // painted origin. A new item starts from `enter_offset` relative to its
        // final position. Size is intentionally not interpolated: changing
        // layout size would reflow the subtree every frame.
        let start_origin = match self.old_bounds.get(id) {
            Some(old) => old.origin,
            None => layout.origin + enter_offset,
        };
        let origin = point(
            start_origin.x + (layout.origin.x - start_origin.x) * progress,
            start_origin.y + (layout.origin.y - start_origin.y) * progress,
        );
        let offset = point(origin.x - layout.origin.x, origin.y - layout.origin.y);

        self.visual_bounds.insert(
            id.clone(),
            Bounds {
                origin,
                size: layout.size,
            },
        );

        MotionFrame {
            offset,
            progress,
            animating: linear < 1.0,
        }
    }
}

/// Wrap a keyed child so it can participate in layout transitions.
///
/// The wrapper deliberately returns the child's own layout id instead of
/// introducing a layout node, so flex/grid placement, gaps and hit-testing
/// behave exactly as before. Only the paint-time origin is shifted.
pub fn motion_item(
    controller: &MotionController,
    id: impl Into<MotionId>,
    child: impl IntoElement,
) -> MotionItem {
    MotionItem {
        controller: controller.clone(),
        id: id.into(),
        child: Some(child.into_any_element()),
        delay: Duration::ZERO,
        enter_offset: Point::default(),
        overlay: false,
        overlay_priority: 0,
    }
}

/// A keyed child that receives a paint-time FLIP offset.
pub struct MotionItem {
    controller: MotionController,
    id: MotionId,
    child: Option<AnyElement>,
    delay: Duration,
    enter_offset: Point<Pixels>,
    overlay: bool,
    overlay_priority: usize,
}

impl MotionItem {
    /// Start this item's part of a staggered transition later than the rest.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// When the item first appears, start this far away from its final
    /// position and settle to zero offset.
    pub fn enter_offset(mut self, offset: Point<Pixels>) -> Self {
        self.enter_offset = offset;
        self
    }

    /// Paint this item after its ancestors while it is animating, so a
    /// cross-parent move is not clipped or covered by a later sibling.
    ///
    /// The layout position still comes from the target parent; only the paint
    /// pass is deferred. Higher priorities paint later.
    pub fn overlay_while_animating(mut self, priority: usize) -> Self {
        self.overlay = true;
        self.overlay_priority = priority;
        self
    }
}

impl IntoElement for MotionItem {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MotionItem {
    type RequestLayoutState = ();
    type PrepaintState = bool;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self
            .child
            .as_mut()
            .expect("MotionItem must only be used once")
            .request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let frame = self.controller.frame(
            &self.id,
            bounds,
            Instant::now(),
            self.delay,
            self.enter_offset,
        );
        if frame.animating {
            window.request_animation_frame();
        }

        let defer = self.overlay && frame.progress < 1.0;
        if defer {
            let child = self
                .child
                .take()
                .expect("MotionItem must only be used once");
            // `defer_draw` prepaints with an absolute element offset. `bounds`
            // already includes the current offset, so add it back to keep the
            // child at the interpolated window position.
            window.defer_draw(
                child,
                window.element_offset() + frame.offset,
                self.overlay_priority,
            );
        } else {
            window.with_element_offset(frame.offset, |window| {
                self.child
                    .as_mut()
                    .expect("MotionItem must only be used once")
                    .prepaint(window, cx);
            });
        }
        defer
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        deferred: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !*deferred {
            self.child
                .as_mut()
                .expect("MotionItem must only be used once")
                .paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};
    use std::time::Duration;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(width), px(height)),
        }
    }

    fn id(value: &str) -> MotionId {
        SharedString::from(value.to_string())
    }

    fn linear_spec(duration: Duration) -> MotionSpec {
        MotionSpec::new(duration, Easing::Linear)
    }

    fn seed(store: &mut MotionStore, id: &MotionId, value: Bounds<Pixels>, now: Instant) {
        let frame = store.frame_at(now, id, value, Duration::ZERO, Point::default());
        assert_eq!(frame.offset, Point::default());
    }

    #[test]
    fn easing_endpoints_and_linear_midpoint() {
        assert_eq!(Easing::Linear.ease(0.0), 0.0);
        assert_eq!(Easing::Linear.ease(1.0), 1.0);
        assert_eq!(Easing::Linear.ease(0.5), 0.5);
        assert_eq!(Easing::EaseOut.ease(0.0), 0.0);
        assert_eq!(Easing::EaseOut.ease(1.0), 1.0);
        assert_eq!(Easing::EaseInOut.ease(0.0), 0.0);
        assert_eq!(Easing::EaseInOut.ease(1.0), 1.0);
    }

    #[test]
    fn ease_out_moves_ahead_of_linear() {
        let value = Easing::EaseOut.ease(0.5);
        assert!(value > 0.5, "ease-out should be past halfway, got {value}");
        assert!(value <= 1.0);
    }

    #[test]
    fn cubic_bezier_is_monotonic_for_ui_curves() {
        let curve = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = curve.ease(step as f32 / 100.0);
            assert!(
                value + 1e-6 >= previous,
                "easing must not go backwards at step {step}"
            );
            previous = value;
        }
    }

    #[test]
    fn begin_requires_a_painted_snapshot() {
        let mut store = MotionStore::default();
        assert!(!store.begin_at(Instant::now(), MotionSpec::LAYOUT));
        assert_eq!(store.started_at, None);

        seed(
            &mut store,
            &id("a"),
            bounds(0.0, 0.0, 10.0, 10.0),
            Instant::now(),
        );
        assert!(store.begin_at(Instant::now(), MotionSpec::LAYOUT));
        assert!(store.started_at.is_some());
    }

    #[test]
    fn enter_progress_distinguishes_known_and_new_items() {
        let now = Instant::now();
        let controller = MotionController::new();
        let existing = id("existing");
        {
            let mut store = controller.inner.borrow_mut();
            seed(&mut store, &existing, bounds(0.0, 0.0, 10.0, 10.0), now);
            assert!(store.begin_at(now, linear_spec(Duration::from_millis(200))));
        }

        assert!(controller.enter_progress(&existing).is_none());
        assert!(
            controller
                .enter_progress(&id("new"))
                .is_some_and(|progress| (0.0..=1.0).contains(&progress))
        );
        assert!((0.0..=1.0).contains(&controller.progress(Duration::ZERO)));
        assert_eq!(MotionController::new().progress(Duration::ZERO), 1.0);
    }

    #[test]
    fn reorder_flips_from_old_to_new_position() {
        let now = Instant::now();
        let mut store = MotionStore::default();
        let a = id("a");
        seed(&mut store, &a, bounds(0.0, 0.0, 10.0, 10.0), now);
        seed(&mut store, &id("b"), bounds(0.0, 30.0, 10.0, 10.0), now);

        assert!(store.begin_at(now, linear_spec(Duration::from_millis(200))));

        let start = store.frame_at(
            now,
            &a,
            bounds(0.0, 30.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert_eq!(start.progress, 0.0);
        assert_eq!(start.offset, point(px(0.0), px(-30.0)));
        assert!(start.animating);

        let middle = store.frame_at(
            now + Duration::from_millis(100),
            &a,
            bounds(0.0, 30.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert!((middle.progress - 0.5).abs() < 1e-6);
        assert_eq!(middle.offset, point(px(0.0), px(-15.0)));
        assert!(middle.animating);

        let end = store.frame_at(
            now + Duration::from_millis(200),
            &a,
            bounds(0.0, 30.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert_eq!(end.progress, 1.0);
        assert_eq!(end.offset, point(px(0.0), px(0.0)));
        assert!(!end.animating);
    }

    #[test]
    fn enter_offset_starts_away_and_settles() {
        let now = Instant::now();
        let mut store = MotionStore::default();
        seed(&mut store, &id("old"), bounds(0.0, 0.0, 10.0, 10.0), now);
        assert!(store.begin_at(now, linear_spec(Duration::from_millis(200))));

        let entering = id("new");
        let start = store.frame_at(
            now,
            &entering,
            bounds(0.0, 100.0, 10.0, 10.0),
            Duration::ZERO,
            point(px(0.0), px(10.0)),
        );
        assert_eq!(start.offset, point(px(0.0), px(10.0)));

        let middle = store.frame_at(
            now + Duration::from_millis(100),
            &entering,
            bounds(0.0, 100.0, 10.0, 10.0),
            Duration::ZERO,
            point(px(0.0), px(10.0)),
        );
        assert_eq!(middle.offset, point(px(0.0), px(5.0)));

        let end = store.frame_at(
            now + Duration::from_millis(200),
            &entering,
            bounds(0.0, 100.0, 10.0, 10.0),
            Duration::ZERO,
            point(px(0.0), px(10.0)),
        );
        assert_eq!(end.offset, point(px(0.0), px(0.0)));
    }

    #[test]
    fn delay_holds_progress_at_zero_then_runs() {
        let now = Instant::now();
        let mut store = MotionStore::default();
        let a = id("a");
        seed(&mut store, &a, bounds(0.0, 0.0, 10.0, 10.0), now);
        assert!(store.begin_at(now, linear_spec(Duration::from_millis(200))));

        let delay = Duration::from_millis(100);
        let during_delay = store.frame_at(
            now + Duration::from_millis(50),
            &a,
            bounds(0.0, 20.0, 10.0, 10.0),
            delay,
            Point::default(),
        );
        assert_eq!(during_delay.progress, 0.0);
        assert_eq!(during_delay.offset, point(px(0.0), px(-20.0)));

        let after_delay = store.frame_at(
            now + Duration::from_millis(150),
            &a,
            bounds(0.0, 20.0, 10.0, 10.0),
            delay,
            Point::default(),
        );
        assert!((after_delay.progress - 0.25).abs() < 1e-6);
        assert_eq!(after_delay.offset, point(px(0.0), px(-15.0)));
    }

    #[test]
    fn second_transition_starts_from_the_painted_position() {
        let now = Instant::now();
        let mut store = MotionStore::default();
        let a = id("a");
        seed(&mut store, &a, bounds(0.0, 0.0, 10.0, 10.0), now);
        assert!(store.begin_at(now, linear_spec(Duration::from_millis(200))));

        let middle = store.frame_at(
            now + Duration::from_millis(100),
            &a,
            bounds(0.0, 20.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert_eq!(middle.offset, point(px(0.0), px(-10.0)));

        let restart = now + Duration::from_millis(100);
        assert!(store.begin_at(restart, linear_spec(Duration::from_millis(200))));
        let restarted = store.frame_at(
            restart,
            &a,
            bounds(0.0, 60.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert_eq!(restarted.offset, point(px(0.0), px(-50.0)));
    }

    #[test]
    fn forget_drops_the_snapshot() {
        let now = Instant::now();
        let controller = MotionController::new();
        let a = id("a");
        {
            let mut store = controller.inner.borrow_mut();
            seed(&mut store, &a, bounds(0.0, 0.0, 10.0, 10.0), now);
            assert!(store.begin_at(now, MotionSpec::LAYOUT));
        }
        assert!(controller.bounds(&a).is_some());
        controller.forget(&a);
        assert!(controller.bounds(&a).is_none());
        assert!(!controller.inner.borrow().old_bounds.contains_key(&a));
    }

    #[test]
    fn zero_duration_finishes_immediately() {
        let now = Instant::now();
        let mut store = MotionStore::default();
        let a = id("a");
        seed(&mut store, &a, bounds(0.0, 0.0, 10.0, 10.0), now);
        assert!(store.begin_at(now, linear_spec(Duration::ZERO)));
        let frame = store.frame_at(
            now,
            &a,
            bounds(0.0, 40.0, 10.0, 10.0),
            Duration::ZERO,
            Point::default(),
        );
        assert_eq!(frame.offset, Point::default());
        assert!(!frame.animating);
    }
}
