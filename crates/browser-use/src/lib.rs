//! EffiBuddy Browser Use: CDP-driven browser control.
//!
//! The crate has no dependency on `agent-core` or the UI. `agent-tools` adapts
//! [`BrowserRuntime`] to a `Tool`; the app only renders the overlay.
//!
//! Layering:
//!
//! ```text
//! runtime   snapshot/stale bookkeeping, policy, screenshot files, pause/stop
//!   ↓
//! cdp       BrowserBackend impl over the Chrome DevTools Protocol
//!   ↓
//! protocol  WebSocket transport with id correlation and event fan-out
//! snapshot  AX tree → [ref] nodes (shared by the CDP and fake backends)
//! journal   console/network ring buffers with cursors
//! discover  engine discovery, launch, DevToolsActivePort attach
//! keys      key-chord parsing → CDP key events
//! ```

pub mod backend;
pub mod discover;
pub mod editor;
pub mod journal;
pub mod keys;
pub mod protocol;
pub mod snapshot;
pub mod types;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub mod cdp;
pub mod runtime;

pub use backend::{
    BrowserBackend, KeyChord, KeyModifier, UnsupportedBackend, WaitCondition, WaitRequest,
};
pub use discover::{EngineCandidate, LaunchedBrowser};
pub use journal::Journal;
pub use runtime::{BrowserRuntime, ConsoleQuery, OverlayEmitter, OverlayEvent, OverlayEventKind};
pub use snapshot::{AX_TREE_JS, PAGE_TEXT_JS, build_snapshot, render_snapshot};
pub use types::{
    AxNode, BrowserError, BrowserInfo, BrowserUseMode, ConsoleEntry, ConsoleLevel, EditorOp,
    EditorRequest, ElementText, EngineKind, EvalOutcome, EvalRequest, JournalPage, JournalQuery,
    MouseButton, NetEntry, PageCapture, PageFrame, PageInfo, Rect, RequestWait, ResponseBody,
    ScreenshotOptions, Snapshot, SnapshotMode, TabInfo, TabInfoState, WaitPolicy,
};

use std::sync::Arc;

/// Build the default backend for this process.
///
/// On every platform this is the CDP backend: it either launches a locally
/// installed Chrome/Edge/Chromium or downloads `chrome-headless-shell` on first
/// use. It never embeds a webview, so the single-native-window architecture of
/// the app is preserved.
pub fn create_backend() -> Arc<dyn BrowserBackend> {
    Arc::new(cdp::CdpBackend::new(discover::default_engine_preference()))
}
