//! The platform backend boundary.
//!
//! A backend only knows how to observe and inject input. Frame
//! bookkeeping, pause/stop and overlay state live in [`crate::runtime`] so all
//! platforms share the same safety behavior.

use async_trait::async_trait;

use crate::keymap::{KeyChord, Modifier};
use crate::types::{
    Capture, ComputerError, DesktopInfo, DisplayInfo, InputStrategy, MouseButton, PlatformInfo,
    PointerTarget, WindowInfo,
};

#[async_trait]
pub trait ComputerBackend: Send + Sync {
    fn platform_info(&self) -> PlatformInfo;

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError>;

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError>;

    /// Look up one window (used for stale-frame validation).
    async fn window_info(&self, window_id: &str) -> Result<WindowInfo, ComputerError> {
        self.list_windows()
            .await?
            .into_iter()
            .find(|window| window.id == window_id)
            .ok_or_else(|| ComputerError::NotFound(format!("window `{window_id}`")))
    }

    async fn capture_screen(
        &self,
        display: Option<&str>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError>;

    async fn capture_window(
        &self,
        window_id: &str,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError>;

    async fn pointer_move(&self, target: &PointerTarget) -> Result<(), ComputerError>;

    async fn pointer_button(
        &self,
        target: &PointerTarget,
        button: MouseButton,
        pressed: bool,
        strategy: InputStrategy,
        modifiers: &[Modifier],
    ) -> Result<(), ComputerError>;

    async fn scroll(
        &self,
        target: &PointerTarget,
        dx: i32,
        dy: i32,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError>;

    async fn type_text(
        &self,
        text: &str,
        window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError>;

    async fn press_key(
        &self,
        chord: &KeyChord,
        repeat: u32,
        hold_ms: u64,
        window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError>;

    async fn activate_window(&self, window_id: &str) -> Result<(), ComputerError>;

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError>;

    /// Claim (or create) an isolated desktop/workspace. On Windows this is the
    /// off-screen "parking" lane; on X11 a dedicated EWMH workspace.
    async fn lock_isolated_desktop(&self, name: &str) -> Result<DesktopInfo, ComputerError>;

    async fn move_window_to_desktop(
        &self,
        window_id: &str,
        desktop_id: &str,
    ) -> Result<(), ComputerError>;

    async fn switch_desktop(&self, desktop_id: &str) -> Result<(), ComputerError>;

    /// Restore one parked/isolated window to its original position.
    async fn restore_window(&self, window_id: &str) -> Result<(), ComputerError>;

    /// Restore every parked window and return from the isolated workspace.
    async fn end_isolation(&self) -> Result<(), ComputerError>;

    /// Give the floating control bar topmost/sticky behavior where the
    /// platform allows it. Best effort.
    async fn pin_overlay(&self, title: &str) -> Result<(), ComputerError>;

    fn supports_isolation(&self) -> bool {
        false
    }
}

/// A backend that compiles everywhere and explains why it cannot act.
pub struct UnsupportedBackend {
    pub info: PlatformInfo,
    pub reason: String,
}

impl UnsupportedBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            info: PlatformInfo {
                platform: std::env::consts::OS.to_string(),
                display_server: "unknown".to_string(),
                backend: "unsupported".to_string(),
                screen_capture: false,
                window_capture: false,
                window_listing: false,
                input: false,
                desktop_isolation: false,
                isolation_kind: None,
                notes: vec![reason.clone()],
            },
            reason,
        }
    }

    fn error<T>(&self) -> Result<T, ComputerError> {
        Err(ComputerError::Unsupported(self.reason.clone()))
    }
}

#[async_trait]
impl ComputerBackend for UnsupportedBackend {
    fn platform_info(&self) -> PlatformInfo {
        self.info.clone()
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        self.error()
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        self.error()
    }

    async fn capture_screen(
        &self,
        _display: Option<&str>,
        _max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        self.error()
    }

    async fn capture_window(
        &self,
        _window_id: &str,
        _max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        self.error()
    }

    async fn pointer_move(&self, _target: &PointerTarget) -> Result<(), ComputerError> {
        self.error()
    }

    async fn pointer_button(
        &self,
        _target: &PointerTarget,
        _button: MouseButton,
        _pressed: bool,
        _strategy: InputStrategy,
        _modifiers: &[Modifier],
    ) -> Result<(), ComputerError> {
        self.error()
    }

    async fn scroll(
        &self,
        _target: &PointerTarget,
        _dx: i32,
        _dy: i32,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.error()
    }

    async fn type_text(
        &self,
        _text: &str,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.error()
    }

    async fn press_key(
        &self,
        _chord: &KeyChord,
        _repeat: u32,
        _hold_ms: u64,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.error()
    }

    async fn activate_window(&self, _window_id: &str) -> Result<(), ComputerError> {
        self.error()
    }

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError> {
        self.error()
    }

    async fn lock_isolated_desktop(&self, _name: &str) -> Result<DesktopInfo, ComputerError> {
        self.error()
    }

    async fn move_window_to_desktop(
        &self,
        _window_id: &str,
        _desktop_id: &str,
    ) -> Result<(), ComputerError> {
        self.error()
    }

    async fn switch_desktop(&self, _desktop_id: &str) -> Result<(), ComputerError> {
        self.error()
    }

    async fn restore_window(&self, _window_id: &str) -> Result<(), ComputerError> {
        self.error()
    }

    async fn end_isolation(&self) -> Result<(), ComputerError> {
        self.error()
    }

    async fn pin_overlay(&self, _title: &str) -> Result<(), ComputerError> {
        self.error()
    }
}
