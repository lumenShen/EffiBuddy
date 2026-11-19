//! Platform-neutral phone shell: the parts of the mobile view that must not
//! depend on a renderer.
//!
//! Two consumers exist today and a third is planned:
//!
//! * `effibuddy-app` renders the phone column on the desktop with GPUI
//!   (the "手机视图" layout, switchable at runtime).
//! * The planned on-device build (`docs/mobile/MOBILE_BUILD.md`) starts from
//!   the same navigation model and tokens so the two shells cannot drift.
//!
//! Nothing here knows about GPUI, windows, pixels-per-point or input events:
//! it is plain state plus numbers, which keeps it testable on the host and
//! portable to whatever draws next (egui/Slint/native).

pub mod layout;
pub mod nav;

pub use layout::Insets;
pub use nav::{
    Filter, Nav, Screen, SessionStatus, SettingsScreen, Sheet, matches_filter, matches_query,
};
