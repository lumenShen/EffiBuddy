//! Shared exponential smooth scrolling for overflow containers.
//!
//! The chat transcript introduced this glide: wheel deltas move a *target*
//! pixel offset, and every frame moves the real offset an exponential fraction
//! of the remaining distance, so repeated notches accumulate into one
//! continuous motion instead of discrete jumps. The state machine lives here
//! so the side bars, the composer and settings feel exactly like the
//! transcript.
//!
//! Two entry points:
//!
//! * [`SmoothScrollHandle`] wraps a GPUI [`ScrollHandle`] for
//!   `overflow_*_scroll` containers (and the base handle of a
//!   `UniformListScrollHandle`). Pair it with [`wheel_capture_overlay`], which
//!   claims wheel events in the capture phase so the built-in discrete handler
//!   never runs; the owning view still has to tick the handle every frame.
//! * [`ScrollGlide`] is the raw state machine for widgets that keep their own
//!   offset, like the chat list and the multiline text input.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{App, Canvas, DispatchPhase, ScrollHandle, ScrollWheelEvent, Window, canvas, point, px};

/// Time constant of the smooth-scroll exponential glide. Smaller = snappier.
pub const SCROLL_GLIDE_TAU: f64 = 0.085;
/// Glide stops once the remaining distance is under this many pixels.
pub const SCROLL_GLIDE_CUTOFF: f32 = 0.5;
/// Pixels per wheel notch/line (same distance the built-in handler used).
pub const WHEEL_LINE_HEIGHT: f32 = 20.0;

/// One frame of the glide, returned by [`ScrollGlide::tick`].
#[derive(Clone, Copy, Debug)]
pub enum GlideTick {
    /// The first tick only records the clock; keep requesting frames.
    Continue,
    /// Apply `delta` (pixels, positive = scroll toward the end) and continue.
    Step(f32),
    /// Apply `delta`, then stop. `at_bottom` marks a `glide_to_bottom`
    /// arrival, which the caller may want to snap.
    Done { delta: f32, at_bottom: bool },
    /// Something else moved the viewport (scrollbar drag, content reset); the
    /// glide cancelled itself and the caller should stop requesting frames.
    Cancelled,
}

/// Exponential glide toward a target offset, measured in pixels from the top.
#[derive(Default, Debug)]
pub struct ScrollGlide {
    target: f32,
    current: f32,
    active: bool,
    last_tick: Option<Instant>,
    goal_bottom: bool,
}

impl ScrollGlide {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// Feed one wheel event. `pixel_y` follows GPUI's wheel convention:
    /// positive = scroll up (toward smaller offsets). `offset` and `max` are
    /// the scrollable's current absolute position and range. Returns whether
    /// the target actually moved (false at a hard edge).
    pub fn wheel(&mut self, pixel_y: f32, offset: f32, max: f32) -> bool {
        if pixel_y == 0.0 {
            return false;
        }
        if !self.active {
            self.current = offset;
            self.target = offset;
            self.last_tick = None;
            self.active = true;
        }
        // A wheel toward the start always owns the viewport; without this the
        // next tick of a `glide_to_bottom` would yank it back to the bottom.
        // The chase kept `target` pinned at `f32::MAX`, so re-base it on the
        // live offset before applying the notch. Wheeling further toward the
        // end keeps the chase: the user already asked for the bottom and the
        // content may still be growing.
        if self.goal_bottom && pixel_y > 0.0 {
            self.goal_bottom = false;
            self.target = offset;
        }
        let previous = self.target;
        self.target = (self.target - pixel_y).clamp(0.0, max);
        // Landing on the maximum offset means the user wheeled all the way to
        // the bottom. Keep chasing it so growth (streaming text) is followed
        // and the owner can re-pin once the glide settles.
        if pixel_y < 0.0 && self.target >= max {
            self.goal_bottom = true;
        }
        self.target != previous
    }

    /// Whether the glide is currently chasing the end of the scrollable.
    pub fn goal_bottom(&self) -> bool {
        self.goal_bottom
    }

    /// Start an animated glide to the bottom (scroll-to-bottom button).
    /// `offset` is the authoritative current position, so the first tick does
    /// not mistake the jump for an external scroll.
    pub fn glide_to_bottom(&mut self, offset: f32, max: f32) {
        self.current = offset;
        self.target = max;
        self.last_tick = None;
        self.active = true;
        self.goal_bottom = true;
    }

    /// Stop any glide and forget the target (scrollbar jump, external scroll).
    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    /// Advance one glide step. Callers apply the returned delta to their
    /// scrollable and call [`commit`](Self::commit) with the read-back offset.
    pub fn tick(&mut self, offset: f32, max: f32) -> GlideTick {
        self.tick_at(Instant::now(), offset, max)
    }

    fn tick_at(&mut self, now: Instant, offset: f32, max: f32) -> GlideTick {
        if !self.active {
            return GlideTick::Cancelled;
        }
        let Some(last) = self.last_tick else {
            self.last_tick = Some(now);
            return GlideTick::Continue;
        };
        let dt = now.duration_since(last).as_secs_f64();
        self.last_tick = Some(now);

        if self.goal_bottom {
            // Chase the live content height; clamped below to the current max.
            self.target = f32::MAX;
        }
        self.target = self.target.clamp(0.0, max);

        if (offset - self.current).abs() > 1.0 {
            // The list quantizes our own motion: `scroll_by` seeks in the
            // height tree and the next layout snaps the logical top to a row
            // boundary, so the read-back can legitimately differ from what we
            // asked for by up to a row. Treating that as "something else moved
            // the viewport" used to reset `target` to the current offset, which
            // threw away every accumulated wheel notch — flicking the wheel ten
            // times moved the list by a single notch.
            //
            // The caller cancels explicitly for the movements that really are
            // external (scrollbar drag, jump to prompt, reset), so the only safe
            // thing to do here is accept the read-back as the new position and
            // keep chasing the target the user asked for.
            self.current = offset;
        }

        let k = 1.0 - (-(dt / SCROLL_GLIDE_TAU)).exp();
        let next = self.current + (self.target - self.current) * (k as f32);
        let delta = next - self.current;
        self.current = next;

        if (self.target - next).abs() <= SCROLL_GLIDE_CUTOFF {
            let at_bottom = self.goal_bottom;
            self.active = false;
            self.goal_bottom = false;
            return GlideTick::Done { delta, at_bottom };
        }
        GlideTick::Step(delta)
    }

    /// Record the offset read back from the scrollable after applying a delta.
    pub fn commit(&mut self, offset: f32) {
        if self.active {
            self.current = offset;
        }
    }
}

/// A [`ScrollHandle`] plus its glide, shared by reference count so render
/// code can hand it to an overlay without borrowing the view.
#[derive(Clone, Default)]
pub struct SmoothScrollHandle {
    handle: ScrollHandle,
    glide: Rc<RefCell<ScrollGlide>>,
}

impl SmoothScrollHandle {
    pub fn new() -> Self {
        Self {
            handle: ScrollHandle::new(),
            glide: Rc::default(),
        }
    }

    /// Wrap an existing handle (for example the base handle of a
    /// `UniformListScrollHandle`, which owns the tracked scroll state).
    pub fn from_handle(handle: ScrollHandle) -> Self {
        Self {
            handle,
            glide: Rc::default(),
        }
    }

    pub fn handle(&self) -> &ScrollHandle {
        &self.handle
    }

    pub fn active(&self) -> bool {
        self.glide.borrow().active()
    }

    pub fn cancel(&self) {
        self.glide.borrow_mut().cancel();
    }

    /// Feed one wheel event. Vertical motion goes through the glide;
    /// horizontal motion applies immediately so two-axis panes (code diffs)
    /// keep their sideways scroll.
    pub fn wheel(&self, pixel_x: f32, pixel_y: f32) {
        let max_y = f32::from(self.handle.max_offset().height);
        let offset_y = -f32::from(self.handle.offset().y);
        if pixel_y != 0.0 {
            self.glide.borrow_mut().wheel(pixel_y, offset_y, max_y);
        }
        if pixel_x != 0.0 {
            let offset = self.handle.offset();
            let max_x = f32::from(self.handle.max_offset().width);
            let next_x = f32::from(offset.x + px(pixel_x)).clamp(-max_x, 0.0);
            self.handle.set_offset(point(px(next_x), offset.y));
        }
    }

    /// Advance one glide step. Returns true while frames are still needed.
    pub fn tick(&self) -> bool {
        if !self.glide.borrow().active() {
            return false;
        }
        let max = f32::from(self.handle.max_offset().height);
        let offset = -f32::from(self.handle.offset().y);
        let outcome = self.glide.borrow_mut().tick(offset, max);
        match outcome {
            GlideTick::Continue => true,
            GlideTick::Step(delta) => {
                self.apply(delta);
                true
            }
            GlideTick::Done { delta, .. } => {
                self.apply(delta);
                false
            }
            GlideTick::Cancelled => false,
        }
    }

    fn apply(&self, delta: f32) {
        if delta.abs() <= 0.01 {
            return;
        }
        let offset = self.handle.offset();
        let next = -f32::from(offset.y) + delta;
        self.handle.set_offset(point(offset.x, px(-next)));
        self.glide
            .borrow_mut()
            .commit(-f32::from(self.handle.offset().y));
    }
}

/// A transparent overlay that claims wheel events inside its bounds and feeds
/// the pixel deltas to `on_wheel` during the capture phase. Paint it as the
/// last child of a `relative()` wrapper around a scrollable: being last means
/// its capture listener runs after nested claim overlays, and claiming stops
/// the scrollable's own bubble-phase handler entirely.
///
/// `on_wheel(pixel_x, pixel_y, window, cx)` returns whether it claimed the
/// event.
pub fn wheel_capture_overlay(
    on_wheel: impl Fn(f32, f32, &mut Window, &mut App) -> bool + 'static,
) -> Canvas<&'static str> {
    canvas(
        |_, _, _| "wheel-capture",
        move |bounds, _, window, _cx| {
            window.on_mouse_event(
                move |event: &ScrollWheelEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != DispatchPhase::Capture || !bounds.contains(&event.position) {
                        return;
                    }
                    let delta = event.delta.pixel_delta(px(WHEEL_LINE_HEIGHT));
                    if delta.x == px(0.0) && delta.y == px(0.0) {
                        return;
                    }
                    if on_wheel(delta.x.into(), delta.y.into(), window, cx) {
                        cx.stop_propagation();
                        window.refresh();
                    }
                },
            );
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Drive a wheel gesture to completion, returning the final offset.
    fn settle(glide: &mut ScrollGlide, mut offset: f32, max: f32, start: Instant) -> f32 {
        assert!(matches!(
            glide.tick_at(start, offset, max),
            GlideTick::Continue
        ));
        for frame in 1..=600 {
            let now = start + Duration::from_millis(16 * frame);
            match glide.tick_at(now, offset, max) {
                GlideTick::Step(delta) => offset += delta,
                GlideTick::Done { delta, .. } => return offset + delta,
                other => panic!("glide ended unexpectedly: {other:?}"),
            }
        }
        panic!("glide did not settle");
    }

    #[test]
    fn wheel_notches_accumulate_and_clamp_at_the_edges() {
        let mut glide = ScrollGlide::new();
        // At the top, scrolling up cannot move the target.
        assert!(!glide.wheel(40.0, 0.0, 1_000.0));
        // Scrolling down starts the glide and accumulates notches.
        assert!(glide.wheel(-40.0, 0.0, 1_000.0));
        assert!(glide.wheel(-40.0, 0.0, 1_000.0));
        assert!(glide.active());
    }

    #[test]
    fn glide_eases_to_the_wheel_target() {
        let start = Instant::now();
        let mut glide = ScrollGlide::new();
        glide.wheel(-100.0, 0.0, 1_000.0);
        let offset = settle(&mut glide, 0.0, 1_000.0, start);
        assert!((offset - 100.0).abs() < 1.0, "offset = {offset}");
        assert!(!glide.active());
    }

    #[test]
    fn external_movement_cancels_the_glide() {
        let start = Instant::now();
        let mut glide = ScrollGlide::new();
        glide.wheel(-100.0, 0.0, 1_000.0);
        assert!(matches!(
            glide.tick_at(start, 0.0, 1_000.0),
            GlideTick::Continue
        ));
        // A scrollbar drag moved the viewport behind the glide's back.
        assert!(matches!(
            glide.tick_at(start + Duration::from_millis(16), 500.0, 1_000.0),
            GlideTick::Cancelled
        ));
        assert!(!glide.active());
    }

    #[test]
    fn a_manual_wheel_cancels_a_bottom_chase() {
        let start = Instant::now();
        let mut glide = ScrollGlide::new();
        glide.glide_to_bottom(0.0, 1_000.0);
        assert!(matches!(
            glide.tick_at(start, 0.0, 1_000.0),
            GlideTick::Continue
        ));
        // One frame of the chase moves toward the bottom.
        let GlideTick::Step(chasing) =
            glide.tick_at(start + Duration::from_millis(16), 0.0, 1_000.0)
        else {
            panic!("expected the bottom chase to move");
        };
        assert!(chasing > 0.0);
        // Wheeling up mid-chase must reverse the motion instead of being
        // dragged back to the bottom by `goal_bottom`.
        glide.wheel(60.0, chasing, 1_000.0);
        let GlideTick::Step(reversed) =
            glide.tick_at(start + Duration::from_millis(32), chasing, 1_000.0)
        else {
            panic!("expected the wheel target to win");
        };
        assert!(reversed < 0.0, "delta = {reversed}");
    }

    #[test]
    fn wheeling_down_to_the_end_reports_arrival_at_the_bottom() {
        let start = Instant::now();
        let mut glide = ScrollGlide::new();
        // One notch overshoots the short scrollable and clamps to its end.
        assert!(glide.wheel(-120.0, 0.0, 100.0));
        assert!(glide.goal_bottom());
        assert!(matches!(
            glide.tick_at(start, 0.0, 100.0),
            GlideTick::Continue
        ));
        let mut offset = 0.0;
        let mut settled = None;
        for frame in 1..=600 {
            let now = start + Duration::from_millis(16 * frame);
            // Content grows while the user is on the way down (streaming).
            let max = if frame < 5 { 100.0 } else { 140.0 };
            match glide.tick_at(now, offset, max) {
                GlideTick::Step(delta) => offset += delta,
                GlideTick::Done { delta, at_bottom } => {
                    offset += delta;
                    settled = Some(at_bottom);
                    break;
                }
                other => panic!("glide ended unexpectedly: {other:?}"),
            }
        }
        assert_eq!(
            settled,
            Some(true),
            "a wheel landing on the end must report `at_bottom` so the caller can re-pin"
        );
        assert!((offset - 140.0).abs() < 1.0, "offset = {offset}");
    }

    #[test]
    fn a_downward_wheel_keeps_the_bottom_chase() {
        let start = Instant::now();
        let mut glide = ScrollGlide::new();
        glide.glide_to_bottom(0.0, 1_000.0);
        assert!(matches!(
            glide.tick_at(start, 0.0, 1_000.0),
            GlideTick::Continue
        ));
        let GlideTick::Step(chasing) =
            glide.tick_at(start + Duration::from_millis(16), 0.0, 1_000.0)
        else {
            panic!("expected the bottom chase to move");
        };
        // The user keeps wheeling toward the end mid-chase: stay on the chase
        // instead of re-basing on the lagging offset.
        glide.wheel(-40.0, chasing, 1_000.0);
        assert!(glide.goal_bottom());
        let GlideTick::Step(next) =
            glide.tick_at(start + Duration::from_millis(32), chasing, 1_000.0)
        else {
            panic!("expected the chase to continue");
        };
        assert!(next > 0.0, "delta = {next}");
    }

    #[test]
    fn wheeling_up_with_nothing_above_does_not_move_or_chase() {
        let mut glide = ScrollGlide::new();
        // At the top there is nowhere to go; the flag must stay unset so the
        // caller keeps its pin state.
        assert!(!glide.wheel(40.0, 0.0, 0.0));
        assert!(!glide.goal_bottom());
    }
}
