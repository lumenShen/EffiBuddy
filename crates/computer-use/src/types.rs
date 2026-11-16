//! Platform-neutral types shared by the backends, the tool adapter and the UI.

use serde::{Deserialize, Serialize};

/// User preference for letting the model read the screen and simulate input.
///
/// This is a *capability* switch, not an approval policy: `Off` keeps the tool
/// out of the registry, and once it is registered the session's
/// `PermissionMode` decides whether a call is approved. The old per-session
/// consent state is gone, so FullAccess reaches the whole agent tree instead of
/// stopping at a one-time card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseMode {
    /// Computer tools are not registered at all.
    Off,
    /// Computer tools are registered and gated by the permission mode.
    #[default]
    #[serde(alias = "session", alias = "always")]
    On,
}

impl ComputerUseMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Bounds {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.right() as f64
            && y < self.bottom() as f64
    }

    pub fn approximately_eq(&self, other: &Self) -> bool {
        (self.x - other.x).abs() <= 2
            && (self.y - other.y).abs() <= 2
            && self.width.abs_diff(other.width) <= 2
            && self.height.abs_diff(other.height) <= 2
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: String,
    pub name: String,
    /// Global screen coordinate of the display's top-left corner.
    pub bounds: Bounds,
    pub scale: f32,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Opaque, stable-enough handle (`0x...` on Windows, X11 window id on X11).
    pub id: String,
    pub title: String,
    pub app: String,
    pub pid: Option<u32>,
    /// Outer window rectangle in global screen coordinates.
    pub bounds: Bounds,
    /// Client-area rectangle. Screenshot frames use this as their origin.
    pub client_bounds: Bounds,
    pub active: bool,
    pub minimized: bool,
    pub desktop_id: Option<String>,
    /// False when the window is known but cannot expose pixels (for example a
    /// hidden/minimized X11 window).
    pub capturable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopInfo {
    pub id: String,
    pub name: String,
    pub index: Option<u32>,
    pub active: bool,
    pub supported: bool,
    /// `workspace`, `parking`, or `current`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub platform: String,
    pub display_server: String,
    pub backend: String,
    pub screen_capture: bool,
    pub window_capture: bool,
    pub window_listing: bool,
    pub input: bool,
    pub desktop_isolation: bool,
    /// Human-readable isolation implementation, if any.
    pub isolation_kind: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn x11_button(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
        }
    }

    pub fn evdev_button(self) -> i32 {
        // Linux input-event-codes: BTN_LEFT/MIDDLE/RIGHT.
        match self {
            Self::Left => 0x110,
            Self::Middle => 0x112,
            Self::Right => 0x111,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputStrategy {
    #[default]
    Auto,
    /// Use window messages only (Windows parked windows).
    Background,
    /// Bring the window forward and use synthetic global input.
    Foreground,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrameTarget {
    Screen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<String>,
    },
    Window {
        window_id: String,
    },
}

/// Everything the model needs to map a pixel in the returned raster back to
/// native coordinates. `origin_*` is the global position of the raster's
/// top-left corner, and `native_*` is the un-scaled target size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub id: String,
    pub target: FrameTarget,
    pub image_width: u32,
    pub image_height: u32,
    pub native_width: u32,
    pub native_height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub desktop_id: Option<String>,
}

impl FrameInfo {
    pub fn scale_x(&self) -> f64 {
        if self.image_width == 0 {
            1.0
        } else {
            self.native_width as f64 / self.image_width as f64
        }
    }

    pub fn scale_y(&self) -> f64 {
        if self.image_height == 0 {
            1.0
        } else {
            self.native_height as f64 / self.image_height as f64
        }
    }

    pub fn window_id(&self) -> Option<&str> {
        match &self.target {
            FrameTarget::Window { window_id } => Some(window_id),
            FrameTarget::Screen { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Capture {
    /// PNG bytes at `frame.image_width × frame.image_height`.
    pub png: Vec<u8>,
    pub frame: FrameInfo,
}

/// A resolved native pointer target. `screen` is in global coordinates;
/// `client` is relative to the target window's client area (or the display
/// origin for screen targets).
#[derive(Debug, Clone, PartialEq)]
pub struct PointerTarget {
    pub screen_x: f64,
    pub screen_y: f64,
    pub client_x: f64,
    pub client_y: f64,
    pub window_id: Option<String>,
    pub background: bool,
    pub desktop_id: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ComputerError {
    #[error("computer use is disabled in settings")]
    Disabled,
    #[error("the user stopped computer control")]
    Stopped,
    #[error("computer control was cancelled")]
    Cancelled,
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("stale frame: {0}. Take a new screenshot before acting.")]
    StaleFrame(String),
    #[error("coordinate is outside the screenshot frame")]
    OutOfBounds,
    #[error("backend error: {0}")]
    Backend(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
}

impl ComputerError {
    /// Stable machine-readable code returned to the model.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "COMPUTER_DISABLED",
            Self::Stopped => "CONTROL_STOPPED",
            Self::Cancelled => "CANCELLED",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::NotFound(_) => "NOT_FOUND",
            Self::StaleFrame(_) => "STALE_FRAME",
            Self::OutOfBounds => "OUT_OF_BOUNDS",
            Self::Backend(_) => "BACKEND_ERROR",
            Self::Timeout(_) => "TIMEOUT",
            Self::Invalid(_) => "INVALID_ARGUMENT",
        }
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ReplyOrIdError> for ComputerError {
    fn from(error: x11rb::errors::ReplyOrIdError) -> Self {
        Self::Backend(error.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectionError> for ComputerError {
    fn from(error: x11rb::errors::ConnectionError) -> Self {
        Self::Backend(error.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ReplyError> for ComputerError {
    fn from(error: x11rb::errors::ReplyError) -> Self {
        Self::Backend(error.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectError> for ComputerError {
    fn from(error: x11rb::errors::ConnectError) -> Self {
        Self::Backend(error.to_string())
    }
}

impl From<std::io::Error> for ComputerError {
    fn from(error: std::io::Error) -> Self {
        Self::Backend(error.to_string())
    }
}
