//! Type layer for computer *browser* use, mirroring `effibuddy-computer`.
//!
//! The model never talks to a browser API directly: it sees an `[ref]`-tagged
//! snapshot, and every action names a ref from a snapshot id. The runtime
//! rejects actions against an evicted or replaced snapshot, so a stale ref can
//! never be replayed onto a different page.
//!
//! Nothing here depends on the UI or on `agent-core`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// User preference for letting the model drive a browser.
///
/// Like [`ComputerUseMode`](effibuddy_computer::ComputerUseMode) this is a
/// capability switch; approval belongs to the session permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrowserUseMode {
    /// Browser tools are not registered at all.
    Off,
    /// Browser tools are registered and gated by the permission mode.
    #[default]
    #[serde(alias = "session", alias = "always")]
    On,
}

impl BrowserUseMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Which executable the session is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Chrome,
    Edge,
    Chromium,
    /// `chrome-headless-shell` downloaded on first use.
    HeadlessShell,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Edge => "Edge",
            Self::Chromium => "Chromium",
            Self::HeadlessShell => "chrome-headless-shell",
        }
    }
}

/// Everything `browser({action:"status"})` needs to explain the setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserInfo {
    /// `None` when no engine was found yet (it is resolved lazily).
    pub engine: Option<EngineKind>,
    pub executable: Option<PathBuf>,
    pub version: Option<String>,
    /// True once a debug endpoint answered and a page session is attached.
    pub connected: bool,
    pub headless: bool,
    /// `ws://127.0.0.1:PORT/devtools/browser/<id>` when attached.
    pub debug_url: Option<String>,
    pub profile_dir: Option<PathBuf>,
    /// Human-readable hints: missing engine, stale profile lock, fallback used.
    pub notes: Vec<String>,
}

impl Default for BrowserInfo {
    fn default() -> Self {
        Self {
            engine: None,
            executable: None,
            version: None,
            connected: false,
            headless: true,
            debug_url: None,
            profile_dir: None,
            notes: Vec::new(),
        }
    }
}

/// Rect in CSS pixels relative to the page's main frame viewport origin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn is_visible(&self) -> bool {
        self.width > 0.5 && self.height > 0.5 && self.x.is_finite() && self.y.is_finite()
    }

    pub fn intersects_viewport(&self, viewport: &Rect) -> bool {
        let left = self.x.max(viewport.x);
        let top = self.y.max(viewport.y);
        let right = (self.x + self.width).min(viewport.x + viewport.width);
        let bottom = (self.y + self.height).min(viewport.y + viewport.height);
        right - left > 0.5 && bottom - top > 0.5
    }
}

/// How much of the accessibility tree a snapshot returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotMode {
    /// Interactive elements plus their names/values. Cheapest useful view.
    #[default]
    Interesting,
    /// Every role the tree reports, including static text.
    Full,
    /// Readable text only, for reading an article instead of clicking.
    Text,
}

/// One addressable element in a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxNode {
    /// Stable handle within this snapshot: `e1`, `e2`, …
    #[serde(rename = "ref")]
    pub id: String,
    /// ARIA/DOM role, lowercased (`button`, `textbox`, `link`, …).
    pub role: String,
    /// Accessible name. Truncated by the snapshotter.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Nesting depth, used by the text renderer.
    pub depth: u32,
    /// Layout box in CSS pixels; `None` when the element has no layout (e.g. an
    /// `<option>`) or could not be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub editable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focusable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// `true`/`false`/`mixed` for checkboxes and radios.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// Target for links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// True when the box was partially or fully outside the viewport at
    /// snapshot time (a click will scroll the element into view first).
    #[serde(default, skip_serializing_if = "is_false")]
    pub offscreen: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The result of one `browser_snapshot` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub url: String,
    pub title: String,
    pub mode: SnapshotMode,
    pub nodes: Vec<AxNode>,
    /// Present for `SnapshotMode::Text` (and as a fallback when the AX tree is
    /// empty), so reading never depends on a visible viewport.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Nodes the tree reported before pruning; `nodes.len()` may be smaller.
    pub total_nodes: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Rect>,
}

impl Snapshot {
    pub fn node(&self, id: &str) -> Option<&AxNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

/// Geometry of one page raster, mirroring `effibuddy-computer`'s frame contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageFrame {
    pub id: String,
    pub image_width: u32,
    pub image_height: u32,
    /// CSS-pixel page size at capture time.
    pub native_width: u32,
    pub native_height: u32,
}

impl PageFrame {
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
}

#[derive(Debug, Clone)]
pub struct PageCapture {
    pub png: Vec<u8>,
    pub frame: PageFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScreenshotOptions {
    /// Clamp the longest edge. Defaults to 1280.
    pub max_dim: Option<u32>,
    pub full_page: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn cdp_button(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    pub fn cdp_mask(self) -> i32 {
        match self {
            Self::Left => 1,
            Self::Middle => 4,
            Self::Right => 2,
        }
    }
}

/// What to wait for after a navigation or an action that may cause one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WaitPolicy {
    /// `load` event, then network quiet (~500ms).
    #[default]
    Load,
    /// Just the DOM's readyState, no network quiescence.
    DomContentLoaded,
    /// Do not wait; useful for single-page apps that never idle.
    None,
}

/// State of the attached tab after an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PageInfo {
    pub tab_id: String,
    pub url: String,
    pub title: String,
    /// `loading` | `interactive` | `complete`
    pub ready_state: String,
    /// Unread console errors since the last `browser_console` call.
    pub console_errors: usize,
    pub pending_requests: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabInfoState {
    Loading,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: String,
    pub url: String,
    pub title: String,
    pub state: TabInfoState,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleLevel {
    Log,
    Debug,
    Info,
    Warning,
    Error,
}

impl ConsoleLevel {
    pub fn is_error(self) -> bool {
        matches!(self, Self::Error)
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Log | Self::Info => 1,
            Self::Warning => 2,
            Self::Error => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    /// Monotonic cursor; pass as `since` to read only newer entries.
    pub seq: u64,
    pub level: ConsoleLevel,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Wall clock, milliseconds since the Unix epoch.
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetEntry {
    pub seq: u64,
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// `document`, `xhr`, `fetch`, `script`, `image`, …
    pub resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub at_ms: i64,
}

impl NetEntry {
    pub fn is_failed(&self) -> bool {
        self.failure.is_some() || self.status.map(|status| status >= 400).unwrap_or(false)
    }
}

/// Cursor-based query over a journal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JournalQuery {
    pub since: Option<u64>,
    pub level: Option<ConsoleLevel>,
    pub failed_only: bool,
    pub url_contains: Option<String>,
    pub limit: Option<usize>,
}

impl JournalQuery {
    pub fn limit_or(&self, default: usize) -> usize {
        self.limit.unwrap_or(default).clamp(1, 500)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JournalPage<T> {
    pub entries: Vec<T>,
    /// Cursor to pass as `since` next time.
    pub next: u64,
    /// Entries the ring buffer dropped before the caller read them.
    pub dropped: u64,
    pub total: u64,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BrowserError {
    #[error("browser use is disabled in settings")]
    Disabled,
    #[error("the user stopped browser control")]
    Stopped,
    #[error("browser control was cancelled")]
    Cancelled,
    #[error("no supported browser engine found: {0}")]
    NoEngine(String),
    #[error("failed to launch or attach to a browser: {0}")]
    Launch(String),
    #[error("devtools protocol error: {0}")]
    Protocol(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("stale snapshot: {0}. Take a new browser_snapshot before acting.")]
    StaleSnapshot(String),
    #[error("stale frame: {0}. Take a new browser_screenshot before clicking coordinates.")]
    StaleFrame(String),
    #[error("target is not interactable: {0}")]
    NotInteractable(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("refused by policy: {0}")]
    Denied(String),
    #[error("page error: {0}")]
    Page(String),
}

impl BrowserError {
    /// Stable, machine-readable code returned to the model.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "BROWSER_DISABLED",
            Self::Stopped => "CONTROL_STOPPED",
            Self::Cancelled => "CANCELLED",
            Self::NoEngine(_) => "NO_ENGINE",
            Self::Launch(_) => "LAUNCH_FAILED",
            Self::Protocol(_) => "PROTOCOL_ERROR",
            Self::Timeout(_) => "TIMEOUT",
            Self::NotFound(_) => "NOT_FOUND",
            Self::StaleSnapshot(_) => "STALE_SNAPSHOT",
            Self::StaleFrame(_) => "STALE_FRAME",
            Self::NotInteractable(_) => "NOT_INTERACTABLE",
            Self::Invalid(_) => "INVALID_ARGUMENT",
            Self::Denied(_) => "DENIED_BY_POLICY",
            Self::Page(_) => "PAGE_ERROR",
        }
    }
}

impl From<std::io::Error> for BrowserError {
    fn from(error: std::io::Error) -> Self {
        Self::Launch(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_on_and_off_disables_registration() {
        assert_eq!(BrowserUseMode::default(), BrowserUseMode::On);
        assert!(BrowserUseMode::On.is_enabled());
        assert!(!BrowserUseMode::Off.is_enabled());
        // Old config files used "session" / "always" for the same state.
        assert_eq!(
            serde_json::from_value::<BrowserUseMode>(serde_json::json!("session")).unwrap(),
            BrowserUseMode::On
        );
        assert_eq!(
            serde_json::from_value::<BrowserUseMode>(serde_json::json!("always")).unwrap(),
            BrowserUseMode::On
        );
    }

    #[test]
    fn page_frame_maps_image_pixels_to_css_pixels() {
        let frame = PageFrame {
            id: "s1".into(),
            image_width: 640,
            image_height: 400,
            native_width: 1280,
            native_height: 800,
        };
        assert_eq!(frame.scale_x(), 2.0);
        assert_eq!(frame.scale_y(), 2.0);
    }

    #[test]
    fn non_finite_coordinates_are_rejected_by_rect_helpers() {
        let rect = Rect::new(f64::NAN, 0.0, 0.0, 0.0);
        assert!(!rect.is_visible());
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        assert!(Rect::new(10.0, 10.0, 5.0, 5.0).intersects_viewport(&viewport));
        assert!(!Rect::new(900.0, 10.0, 5.0, 5.0).intersects_viewport(&viewport));
    }

    #[test]
    fn net_entry_treats_4xx_as_failure() {
        let mut entry = NetEntry {
            seq: 1,
            method: "GET".into(),
            url: "https://example.com".into(),
            status: Some(200),
            resource_type: "document".into(),
            failure: None,
            duration_ms: None,
            at_ms: 0,
        };
        assert!(!entry.is_failed());
        entry.status = Some(404);
        assert!(entry.is_failed());
        entry.status = None;
        entry.failure = Some("net::ERR_FAILED".into());
        assert!(entry.is_failed());
    }

    #[test]
    fn journal_query_limit_is_clamped() {
        let query = JournalQuery::default();
        assert_eq!(query.limit_or(40), 40);
        assert_eq!(
            JournalQuery {
                limit: Some(0),
                ..JournalQuery::default()
            }
            .limit_or(40),
            1
        );
        assert_eq!(
            JournalQuery {
                limit: Some(9999),
                ..JournalQuery::default()
            }
            .limit_or(40),
            500
        );
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(BrowserError::NoEngine("x".into()).code(), "NO_ENGINE");
        assert_eq!(
            BrowserError::StaleSnapshot("x".into()).code(),
            "STALE_SNAPSHOT"
        );
        assert_eq!(BrowserError::Denied("x".into()).code(), "DENIED_BY_POLICY");
    }
}
