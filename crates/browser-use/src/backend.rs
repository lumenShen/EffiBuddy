//! The browser backend boundary.
//!
//! A backend knows how to talk to one browser engine. Snapshot
//! bookkeeping, policy checks, pause/stop and screenshot persistence live in
//! [`crate::runtime`], so every backend (CDP, fake, future WebKit bridge)
//! shares the same safety behavior.
//!
//! All coordinates are CSS pixels in the main frame's viewport, matching what
//! [`crate::types::Rect`] reports from the accessibility tree.

use async_trait::async_trait;
use serde_json::Value;

use crate::types::{
    AxNode, BrowserError, BrowserInfo, ConsoleEntry, JournalPage, JournalQuery, MouseButton,
    NetEntry, PageCapture, PageInfo, ScreenshotOptions, Snapshot, SnapshotMode, TabInfo,
    WaitPolicy,
};

/// One keyboard chord, e.g. `ctrl+shift+t`, `Return`, `Escape`, `F5`.
///
/// Parsing lives in [`crate::keys`] so the tool adapter and backends agree on
/// the accepted syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub modifiers: Vec<KeyModifier>,
    /// Normalized key name (`t`, `enter`, `escape`, `f5`, `arrowdown`).
    pub key: String,
}

impl KeyChord {
    pub fn has(&self, modifier: KeyModifier) -> bool {
        self.modifiers.contains(&modifier)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyModifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl KeyModifier {
    /// CDP modifier bitmask used by `Input.dispatch*Event`.
    pub fn cdp_mask(self) -> i32 {
        match self {
            Self::Alt => 1,
            Self::Ctrl => 2,
            // 4 is the Meta/"super" bit in Chromium's input pipeline.
            Self::Super => 4,
            Self::Shift => 8,
        }
    }
}

/// What a `wait_for` call should block on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitCondition {
    /// DOM readyState reached `complete` (or `interactive` for
    /// `DomContentLoaded`) and the network stayed quiet.
    Settled,
    /// A URL substring appears in the current tab.
    UrlContains,
    /// A snapshot ref exists and is visible.
    RefVisible,
    /// A CSS selector matches a visible element.
    SelectorVisible,
    /// Console errors appeared after the given cursor.
    ConsoleError,
}

#[derive(Debug, Clone)]
pub struct WaitRequest {
    pub condition: WaitCondition,
    /// Argument for the condition (substring, ref, selector). Ignored by
    /// `Settled` and `ConsoleError`.
    pub argument: Option<String>,
    pub timeout_ms: u64,
    /// Console cursor for `ConsoleError`; `None` uses errors since the last
    /// query.
    pub since: Option<u64>,
}

/// A backend that drives one browser engine.
///
/// Contract notes for implementors:
///
/// - Every method is safe to call concurrently; the backend serializes CDP
///   traffic internally.
/// - `ensure_ready` must be idempotent: the runtime calls it before anything
///   that needs a live page.
/// - Ref-based actions must resolve the ref through the *latest* snapshot the
///   backend produced; the runtime has already validated the snapshot id, so a
///   missing ref means the page changed under the model's feet.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Cheap, allocation-only status. Must never launch a browser.
    fn info(&self) -> BrowserInfo;

    /// Launch or attach, enable the event domains, and return the active tab.
    async fn ensure_ready(&self) -> Result<PageInfo, BrowserError>;

    /// Close the browser (or detach) and release the profile lock.
    async fn end_session(&self) -> Result<(), BrowserError>;

    /// Navigate the active tab and wait according to `wait`.
    async fn navigate(&self, url: &str, wait: WaitPolicy) -> Result<PageInfo, BrowserError>;

    /// Reload the active tab.
    async fn reload(&self, wait: WaitPolicy) -> Result<PageInfo, BrowserError>;

    /// `Page.navigate` history moves.
    async fn history(&self, back: bool) -> Result<PageInfo, BrowserError>;

    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError>;

    async fn new_tab(&self, url: Option<&str>) -> Result<TabInfo, BrowserError>;

    async fn select_tab(&self, tab_id: &str) -> Result<PageInfo, BrowserError>;

    async fn close_tab(&self, tab_id: &str) -> Result<(), BrowserError>;

    /// Build an `[ref]`-tagged accessibility snapshot of the active tab.
    async fn snapshot(
        &self,
        mode: SnapshotMode,
        max_nodes: usize,
    ) -> Result<Snapshot, BrowserError>;

    /// Raw text extraction, used for `SnapshotMode::Text` and as a fallback.
    async fn page_text(&self, max_chars: usize) -> Result<String, BrowserError>;

    async fn screenshot(&self, options: ScreenshotOptions) -> Result<PageCapture, BrowserError>;

    async fn click_ref(
        &self,
        r#ref: &str,
        button: MouseButton,
        click_count: u32,
        modifiers: &[KeyModifier],
        wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError>;

    /// Click a pixel of the most recent [`PageCapture`] raster.
    async fn click_frame_pixel(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError>;

    async fn hover_ref(&self, r#ref: &str) -> Result<PageInfo, BrowserError>;

    async fn type_ref(
        &self,
        r#ref: &str,
        text: &str,
        submit: bool,
        wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError>;

    /// Type into whatever is focused.
    async fn type_text(&self, text: &str) -> Result<PageInfo, BrowserError>;

    async fn press_key(&self, chord: &KeyChord) -> Result<PageInfo, BrowserError>;

    /// Scroll the element's nearest scrollable ancestor, or the page.
    async fn scroll(&self, r#ref: Option<&str>, dx: i32, dy: i32)
    -> Result<PageInfo, BrowserError>;

    async fn select_option(&self, r#ref: &str, value: &str) -> Result<PageInfo, BrowserError>;

    async fn set_checked(&self, r#ref: &str, checked: bool) -> Result<PageInfo, BrowserError>;

    /// Attach files to a file input. The runtime has already checked that the
    /// path is inside the workspace and that the user approved the upload.
    async fn upload_file(
        &self,
        r#ref: &str,
        path: &std::path::Path,
    ) -> Result<PageInfo, BrowserError>;

    /// Evaluate an expression in the page. The runtime treats this as a
    /// high-risk action and gates it behind an explicit approval.
    async fn evaluate(&self, expression: &str) -> Result<Value, BrowserError>;

    async fn console(
        &self,
        query: &JournalQuery,
    ) -> Result<JournalPage<ConsoleEntry>, BrowserError>;

    async fn network(&self, query: &JournalQuery) -> Result<JournalPage<NetEntry>, BrowserError>;

    async fn wait_for(&self, request: &WaitRequest) -> Result<PageInfo, BrowserError>;

    /// Current tab state without round-tripping the page.
    async fn page_info(&self) -> Result<PageInfo, BrowserError>;

    /// Resolve a ref against the *current* snapshot to its box, for error
    /// messages and for scrolling into view.
    async fn ref_bounds(&self, r#ref: &str) -> Result<Option<crate::types::Rect>, BrowserError>;

    /// The most recent ref node, used when the tool needs role/name context.
    async fn ref_node(&self, r#ref: &str) -> Result<Option<AxNode>, BrowserError>;
}

/// A backend that compiles everywhere and explains why it cannot act.
pub struct UnsupportedBackend {
    pub info: BrowserInfo,
    pub reason: String,
}

impl UnsupportedBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let mut info = BrowserInfo::default();
        info.notes.push(reason.clone());
        Self { info, reason }
    }

    fn error<T>(&self) -> Result<T, BrowserError> {
        Err(BrowserError::NoEngine(self.reason.clone()))
    }
}

#[async_trait]
impl BrowserBackend for UnsupportedBackend {
    fn info(&self) -> BrowserInfo {
        self.info.clone()
    }

    async fn ensure_ready(&self) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn end_session(&self) -> Result<(), BrowserError> {
        Ok(())
    }

    async fn navigate(&self, _url: &str, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn reload(&self, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn history(&self, _back: bool) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
        self.error()
    }

    async fn new_tab(&self, _url: Option<&str>) -> Result<TabInfo, BrowserError> {
        self.error()
    }

    async fn select_tab(&self, _tab_id: &str) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn close_tab(&self, _tab_id: &str) -> Result<(), BrowserError> {
        self.error()
    }

    async fn snapshot(
        &self,
        _mode: SnapshotMode,
        _max_nodes: usize,
    ) -> Result<Snapshot, BrowserError> {
        self.error()
    }

    async fn page_text(&self, _max_chars: usize) -> Result<String, BrowserError> {
        self.error()
    }

    async fn screenshot(&self, _options: ScreenshotOptions) -> Result<PageCapture, BrowserError> {
        self.error()
    }

    async fn click_ref(
        &self,
        _ref: &str,
        _button: MouseButton,
        _click_count: u32,
        _modifiers: &[KeyModifier],
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn click_frame_pixel(
        &self,
        _x: f64,
        _y: f64,
        _button: MouseButton,
        _click_count: u32,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn hover_ref(&self, _ref: &str) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn type_ref(
        &self,
        _ref: &str,
        _text: &str,
        _submit: bool,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn type_text(&self, _text: &str) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn press_key(&self, _chord: &KeyChord) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn scroll(
        &self,
        _ref: Option<&str>,
        _dx: i32,
        _dy: i32,
    ) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn select_option(&self, _ref: &str, _value: &str) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn set_checked(&self, _ref: &str, _checked: bool) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn upload_file(
        &self,
        _ref: &str,
        _path: &std::path::Path,
    ) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn evaluate(&self, _expression: &str) -> Result<Value, BrowserError> {
        self.error()
    }

    async fn console(
        &self,
        _query: &JournalQuery,
    ) -> Result<JournalPage<ConsoleEntry>, BrowserError> {
        self.error()
    }

    async fn network(&self, _query: &JournalQuery) -> Result<JournalPage<NetEntry>, BrowserError> {
        self.error()
    }

    async fn wait_for(&self, _request: &WaitRequest) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn page_info(&self) -> Result<PageInfo, BrowserError> {
        self.error()
    }

    async fn ref_bounds(&self, _ref: &str) -> Result<Option<crate::types::Rect>, BrowserError> {
        self.error()
    }

    async fn ref_node(&self, _ref: &str) -> Result<Option<AxNode>, BrowserError> {
        self.error()
    }
}
