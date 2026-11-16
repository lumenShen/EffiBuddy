//! Deterministic backend used by unit tests and the UI preview mode.
//!
//! Placeholder until the implementation lands.

use async_trait::async_trait;
use serde_json::Value;

use crate::backend::{BrowserBackend, KeyChord, KeyModifier, WaitRequest};
use crate::types::{
    AxNode, BrowserError, BrowserInfo, ConsoleEntry, JournalPage, JournalQuery, MouseButton,
    NetEntry, PageCapture, PageInfo, Rect, ScreenshotOptions, Snapshot, SnapshotMode, TabInfo,
    WaitPolicy,
};

#[derive(Default)]
pub struct FakeBrowser;

impl FakeBrowser {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BrowserBackend for FakeBrowser {
    fn info(&self) -> BrowserInfo {
        BrowserInfo {
            engine: Some(crate::types::EngineKind::Chromium),
            executable: Some(std::path::PathBuf::from("/fake/chromium")),
            version: Some("Fake/1.0".to_string()),
            connected: true,
            headless: true,
            debug_url: Some("ws://127.0.0.1:0/devtools/browser/fake".to_string()),
            profile_dir: None,
            notes: vec!["fake backend".to_string()],
        }
    }

    async fn ensure_ready(&self) -> Result<PageInfo, BrowserError> {
        Ok(PageInfo {
            tab_id: "tab-1".to_string(),
            url: "https://example.test/".to_string(),
            title: "Fake Page".to_string(),
            ready_state: "complete".to_string(),
            console_errors: 0,
            pending_requests: 0,
        })
    }

    async fn end_session(&self) -> Result<(), BrowserError> {
        Ok(())
    }

    async fn navigate(&self, url: &str, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        Ok(PageInfo {
            tab_id: "tab-1".to_string(),
            url: url.to_string(),
            title: "Fake Page".to_string(),
            ready_state: "complete".to_string(),
            console_errors: 0,
            pending_requests: 0,
        })
    }

    async fn reload(&self, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn history(&self, _back: bool) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
        Ok(vec![TabInfo {
            id: "tab-1".to_string(),
            url: "https://example.test/".to_string(),
            title: "Fake Page".to_string(),
            state: crate::types::TabInfoState::Ready,
            active: true,
        }])
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<TabInfo, BrowserError> {
        Ok(TabInfo {
            id: "tab-1".to_string(),
            url: url.unwrap_or("about:blank").to_string(),
            title: "Fake Page".to_string(),
            state: crate::types::TabInfoState::Ready,
            active: true,
        })
    }

    async fn select_tab(&self, _tab_id: &str) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn close_tab(&self, _tab_id: &str) -> Result<(), BrowserError> {
        Ok(())
    }

    async fn snapshot(
        &self,
        mode: SnapshotMode,
        max_nodes: usize,
    ) -> Result<Snapshot, BrowserError> {
        let _ = max_nodes;
        Ok(Snapshot {
            id: String::new(),
            url: "https://example.test/".to_string(),
            title: "Fake Page".to_string(),
            mode,
            nodes: Vec::new(),
            text: None,
            total_nodes: 0,
            truncated: false,
            viewport: None,
        })
    }

    async fn page_text(&self, _max_chars: usize) -> Result<String, BrowserError> {
        Ok("Fake page text".to_string())
    }

    async fn screenshot(&self, _options: ScreenshotOptions) -> Result<PageCapture, BrowserError> {
        Err(BrowserError::Launch(
            "fake backend has no raster".to_string(),
        ))
    }

    async fn click_ref(
        &self,
        _ref: &str,
        _button: MouseButton,
        _click_count: u32,
        _modifiers: &[KeyModifier],
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn click_frame_pixel(
        &self,
        _x: f64,
        _y: f64,
        _button: MouseButton,
        _click_count: u32,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn hover_ref(&self, _ref: &str) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn type_ref(
        &self,
        _ref: &str,
        _text: &str,
        _submit: bool,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn type_text(&self, _text: &str) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn press_key(&self, _chord: &KeyChord) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn scroll(
        &self,
        _ref: Option<&str>,
        _dx: i32,
        _dy: i32,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn select_option(&self, _ref: &str, _value: &str) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn set_checked(&self, _ref: &str, _checked: bool) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn upload_file(
        &self,
        _ref: &str,
        _path: &std::path::Path,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn evaluate(&self, _expression: &str) -> Result<Value, BrowserError> {
        Ok(Value::Null)
    }

    async fn console(
        &self,
        _query: &JournalQuery,
    ) -> Result<JournalPage<ConsoleEntry>, BrowserError> {
        Ok(JournalPage {
            entries: Vec::new(),
            next: 0,
            dropped: 0,
            total: 0,
        })
    }

    async fn network(&self, _query: &JournalQuery) -> Result<JournalPage<NetEntry>, BrowserError> {
        Ok(JournalPage {
            entries: Vec::new(),
            next: 0,
            dropped: 0,
            total: 0,
        })
    }

    async fn wait_for(&self, _request: &WaitRequest) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn page_info(&self) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn ref_bounds(&self, _ref: &str) -> Result<Option<Rect>, BrowserError> {
        Ok(None)
    }

    async fn ref_node(&self, _ref: &str) -> Result<Option<AxNode>, BrowserError> {
        Ok(None)
    }
}
