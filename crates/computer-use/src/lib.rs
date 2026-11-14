//! EffiBuddy Computer Use: platform backends + shared safety runtime.
//!
//! The crate deliberately has no dependency on `agent-core` or the UI. The
//! `agent-tools` crate adapts [`ComputerRuntime`] to a `Tool`, and the app
//! renders [`runtime::OverlayEvent`] as the floating control bar.

pub mod backend;
pub mod encode;
pub mod frame;
pub mod keymap;
pub mod runtime;
pub mod types;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

pub use backend::{ComputerBackend, UnsupportedBackend};
pub use frame::{FrameRegistry, map_frame};
pub use keymap::{KeyChord, Modifier, parse_chord, parse_modifiers};
pub use runtime::{
    CaptureTarget, ClickRequest, ComputerRuntime, DragRequest, KeyRequest, MoveRequest,
    OverlayEmitter, OverlayEvent, OverlayEventKind, ScrollRequest, TypeRequest,
};
pub use types::{
    Bounds, Capture, ComputerError, ComputerUseMode, DesktopInfo, DisplayInfo, FrameInfo,
    FrameTarget, InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

use std::sync::Arc;

/// Pick the native backend for this process.
///
/// Linux prefers Wayland when a Wayland session is detected, because XWayland
/// only exposes X11 clients and cannot capture native Wayland windows. Set
/// `EFFIBUDDY_COMPUTER_BACKEND=x11` to force the X11 backend for testing.
pub fn create_backend() -> Arc<dyn ComputerBackend> {
    #[cfg(target_os = "linux")]
    {
        return linux::create_backend();
    }
    #[cfg(target_os = "windows")]
    {
        return Arc::new(windows::WindowsBackend::new());
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Arc::new(UnsupportedBackend::new(
            "Computer Use currently supports Linux (X11/Wayland) and Windows.",
        ))
    }
}
