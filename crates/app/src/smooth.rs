//! Smooth-scroll handles for the app's overflow regions.
//!
//! The chat transcript's glide was extracted into `effibuddy_ui::scroll`;
//! this module owns one [`SmoothScrollHandle`] per non-chat scroll container
//! so the left sidebar, the right side pane, the settings page and the model
//! editor all scroll with the same exponential easing.
//!
//! Ticking happens once per frame in `AppView::render`: an active glide writes
//! the next offset into its tracked `ScrollHandle`, and the app schedules a
//! follow-up frame until every glide settles.

use effibuddy_ui::{SmoothScrollHandle, wheel_capture_overlay};
use gpui::{Canvas, UniformListScrollHandle, WeakEntity};

use crate::app::AppView;

/// Capture-phase wheel overlay that drives one smooth-scroll handle.
///
/// The handle writes the target immediately, but the exponential steps run in
/// `AppView::render`; notifying the view here guarantees that render (and
/// therefore `SmoothScrollers::tick`) actually runs on the frame the wheel
/// request triggers. Without the notify, `window.refresh()` alone could repaint
/// the cached tree and the glide would never advance.
pub(crate) fn smooth_overlay(
    handle: SmoothScrollHandle,
    entity: WeakEntity<AppView>,
) -> Canvas<&'static str> {
    wheel_capture_overlay(move |pixel_x, pixel_y, _window, cx| {
        handle.wheel(pixel_x, pixel_y);
        let _ = entity.update(cx, |_, cx| cx.notify());
        true
    })
}

/// Every tracked scroll region outside the chat transcript.
pub(crate) struct SmoothScrollers {
    /// Left sidebar session list (`#session-list`).
    pub sessions: SmoothScrollHandle,
    /// Right side pane: plan tab.
    pub side_plan: SmoothScrollHandle,
    /// Right side pane: agents tab.
    pub side_agents: SmoothScrollHandle,
    /// Right side pane: changes tab file list.
    pub side_git_files: SmoothScrollHandle,
    /// Right side pane: changes tab diff body (two-axis; horizontal is
    /// applied immediately by the shared handle).
    pub side_git_diff: SmoothScrollHandle,
    /// Side pane overview popover.
    pub side_overview: SmoothScrollHandle,
    /// Settings content column.
    pub settings: SmoothScrollHandle,
    /// Add/edit model dialog.
    pub model_editor: SmoothScrollHandle,
    /// Right side pane: file tree (`uniform_list`).
    pub file_tree: SmoothScrollHandle,
    /// Right side pane: file preview (`uniform_list`).
    pub file_preview: SmoothScrollHandle,
    /// Handles the two `uniform_list`s need at render time.
    pub file_tree_list: UniformListScrollHandle,
    pub file_preview_list: UniformListScrollHandle,
}

impl SmoothScrollers {
    pub fn new() -> Self {
        let file_tree_list = UniformListScrollHandle::new();
        let file_preview_list = UniformListScrollHandle::new();
        let file_tree =
            SmoothScrollHandle::from_handle(file_tree_list.0.borrow().base_handle.clone());
        let file_preview =
            SmoothScrollHandle::from_handle(file_preview_list.0.borrow().base_handle.clone());
        Self {
            sessions: SmoothScrollHandle::new(),
            side_plan: SmoothScrollHandle::new(),
            side_agents: SmoothScrollHandle::new(),
            side_git_files: SmoothScrollHandle::new(),
            side_git_diff: SmoothScrollHandle::new(),
            side_overview: SmoothScrollHandle::new(),
            settings: SmoothScrollHandle::new(),
            model_editor: SmoothScrollHandle::new(),
            file_tree,
            file_preview,
            file_tree_list,
            file_preview_list,
        }
    }

    /// Advance every active glide. Returns true while at least one of them
    /// still wants frames.
    pub fn tick(&self) -> bool {
        let mut needs_frames = false;
        for handle in [
            &self.sessions,
            &self.side_plan,
            &self.side_agents,
            &self.side_git_files,
            &self.side_git_diff,
            &self.side_overview,
            &self.settings,
            &self.model_editor,
            &self.file_tree,
            &self.file_preview,
        ] {
            needs_frames |= handle.tick();
        }
        needs_frames
    }
}
