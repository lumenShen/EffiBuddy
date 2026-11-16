//! Deterministic backend used by unit tests and the UI preview mode.
//!
//! Placeholder until the implementation lands.

use async_trait::async_trait;
use serde_json::Value;

use crate::backend::{BrowserBackend, KeyChord, KeyModifier, WaitRequest};

use crate::types::{
    AxNode, BrowserError, BrowserInfo, ConsoleEntry, EditorOp, EditorRequest, ElementText,
    EvalOutcome, EvalRequest, JournalPage, JournalQuery, MouseButton, NetEntry, PageCapture,
    PageInfo, Rect, RequestWait, ResponseBody, ScreenshotOptions, Snapshot, SnapshotMode, TabInfo,
    WaitPolicy,
};

/// One fake tab row.
#[derive(Debug, Clone)]
struct FakeTab {
    id: String,
    url: String,
    title: String,
}

#[derive(Default)]
struct FakeTabs {
    next: usize,
    tabs: Vec<FakeTab>,
    active: Option<String>,
}

impl FakeTabs {
    fn seeded() -> Self {
        Self {
            next: 2,
            tabs: vec![FakeTab {
                id: "tab-1".to_string(),
                url: "https://example.test/".to_string(),
                title: "Fake Page".to_string(),
            }],
            active: Some("tab-1".to_string()),
        }
    }

    fn active(&self) -> FakeTab {
        self.tabs
            .iter()
            .find(|tab| Some(tab.id.as_str()) == self.active.as_deref())
            .cloned()
            .unwrap_or_else(|| FakeTab {
                id: "tab-1".to_string(),
                url: "https://example.test/".to_string(),
                title: "Fake Page".to_string(),
            })
    }

    fn page_info(&self) -> PageInfo {
        let tab = self.active();
        PageInfo {
            tab_id: tab.id,
            url: tab.url,
            title: tab.title,
            ready_state: "complete".to_string(),
            console_errors: 0,
            pending_requests: 0,
        }
    }
}

/// Fake backend with real multi-tab bookkeeping, so the per-conversation tab
/// binding can be tested without a browser.
pub struct FakeBrowser {
    tabs: std::sync::Mutex<FakeTabs>,
    init_scripts: std::sync::Mutex<Vec<String>>,
}

impl Default for FakeBrowser {
    fn default() -> Self {
        Self {
            tabs: std::sync::Mutex::new(FakeTabs::seeded()),
            init_scripts: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl FakeBrowser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tab the fake backend is currently attached to.
    pub fn active_tab(&self) -> Option<String> {
        self.lock().active.clone()
    }

    /// Scripts the runtime installed for the currently attached tab.
    pub fn init_scripts(&self) -> Vec<String> {
        self.init_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn tab_ids(&self) -> Vec<String> {
        self.lock().tabs.iter().map(|tab| tab.id.clone()).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeTabs> {
        self.tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        Ok(self.lock().page_info())
    }

    async fn end_session(&self) -> Result<(), BrowserError> {
        Ok(())
    }

    async fn navigate(&self, url: &str, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        let mut tabs = self.lock();
        let active = tabs.active.clone();
        if let Some(tab) = tabs
            .tabs
            .iter_mut()
            .find(|tab| Some(tab.id.as_str()) == active.as_deref())
        {
            tab.url = url.to_string();
        }
        Ok(tabs.page_info())
    }

    async fn reload(&self, _wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn history(&self, _back: bool) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
        let tabs = self.lock();
        Ok(tabs
            .tabs
            .iter()
            .map(|tab| TabInfo {
                id: tab.id.clone(),
                url: tab.url.clone(),
                title: tab.title.clone(),
                state: crate::types::TabInfoState::Ready,
                active: Some(tab.id.as_str()) == tabs.active.as_deref(),
            })
            .collect())
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<TabInfo, BrowserError> {
        let mut tabs = self.lock();
        let id = format!("tab-{}", tabs.next);
        tabs.next += 1;
        let url = url.unwrap_or("about:blank").to_string();
        tabs.tabs.push(FakeTab {
            id: id.clone(),
            url: url.clone(),
            title: "Fake Page".to_string(),
        });
        tabs.active = Some(id.clone());
        Ok(TabInfo {
            id,
            url,
            title: "Fake Page".to_string(),
            state: crate::types::TabInfoState::Ready,
            active: true,
        })
    }

    async fn select_tab(&self, tab_id: &str) -> Result<PageInfo, BrowserError> {
        let mut tabs = self.lock();
        if !tabs.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(BrowserError::NotFound(format!("no tab `{tab_id}`")));
        }
        tabs.active = Some(tab_id.to_string());
        Ok(tabs.page_info())
    }

    async fn close_tab(&self, tab_id: &str) -> Result<(), BrowserError> {
        let mut tabs = self.lock();
        if Some(tab_id) == tabs.active.as_deref() {
            return Err(BrowserError::Invalid(
                "cannot close the attached tab".to_string(),
            ));
        }
        tabs.tabs.retain(|tab| tab.id != tab_id);
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
            nodes: fake_nodes(),
            text: Some("Fake page text".to_string()),
            total_nodes: 2,
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

    async fn evaluate(&self, request: &EvalRequest) -> Result<EvalOutcome, BrowserError> {
        Ok(EvalOutcome {
            value: if request.expression.contains("timeout") {
                Value::Null
            } else {
                Value::String(format!("fake: {}", request.expression))
            },
            timed_out: request.expression.contains("timeout"),
            logs: vec!["fake log line".to_string()],
        })
    }

    async fn response_body(
        &self,
        request_id: &str,
        max_chars: usize,
    ) -> Result<ResponseBody, BrowserError> {
        Ok(ResponseBody {
            request_id: request_id.to_string(),
            url: format!("https://example.test/{request_id}"),
            method: "GET".to_string(),
            status: Some(200),
            content_type: Some("application/json".to_string()),
            body: "{}".chars().take(max_chars).collect(),
            truncated: false,
            base64: false,
            available: true,
            note: None,
        })
    }

    async fn wait_for_request(&self, request: &RequestWait) -> Result<ResponseBody, BrowserError> {
        if request.timeout_ms < 150 {
            return Err(BrowserError::Timeout(format!(
                "no request matched `{}`",
                request.pattern
            )));
        }
        self.response_body("fake-request", request.max_chars).await
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
        Ok(fake_nodes()
            .iter()
            .find(|node| node.id == _ref)
            .and_then(|node| node.bounds))
    }

    async fn ref_node(&self, r#ref: &str) -> Result<Option<AxNode>, BrowserError> {
        Ok(fake_nodes().into_iter().find(|node| node.id == r#ref))
    }

    async fn element_text(
        &self,
        selector: &str,
        _all: bool,
        max_chars: usize,
    ) -> Result<ElementText, BrowserError> {
        Ok(ElementText {
            selector: selector.to_string(),
            matches: 1,
            text: format!("fake text for {selector}")
                .chars()
                .take(max_chars)
                .collect(),
            truncated: false,
        })
    }

    async fn element_box(&self, selector: &str) -> Result<Option<Rect>, BrowserError> {
        if selector.contains("missing") {
            return Ok(None);
        }
        Ok(Some(Rect::new(10.0, 20.0, 30.0, 40.0)))
    }

    async fn click_point(
        &self,
        _x: f64,
        _y: f64,
        _button: MouseButton,
        _click_count: u32,
        _modifiers: &[KeyModifier],
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn drag(
        &self,
        _from: (f64, f64),
        _to: (f64, f64),
        _steps: u32,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await
    }

    async fn editor(&self, request: &EditorRequest) -> Result<Value, BrowserError> {
        Ok(serde_json::json!({
            "ok": true,
            "kind": "textarea",
            "op": request.op.as_str(),
            "target": request.target,
            "files": [{
                "index": 0,
                "target": "src/main.cpp",
                "kind": "textarea",
                "chars": request.content.as_deref().map(|text| text.chars().count()).unwrap_or(0),
                "current": true,
                "dirty": false,
            }],
            "value": request.content.clone().unwrap_or_else(|| "fake buffer".to_string()),
            "saved": request.op == EditorOp::Save,
            "dirty_after_save": false,
        }))
    }

    async fn set_init_scripts(&self, scripts: &[String]) -> Result<(), BrowserError> {
        *self
            .init_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = scripts.to_vec();
        Ok(())
    }
}

/// Two nodes so ref-based flows can be exercised without a browser.
fn fake_nodes() -> Vec<AxNode> {
    vec![
        AxNode {
            id: "e1".to_string(),
            role: "button".to_string(),
            name: "Save".to_string(),
            value: None,
            depth: 1,
            bounds: Some(Rect::new(10.0, 20.0, 30.0, 40.0)),
            editable: false,
            focusable: true,
            disabled: false,
            checked: None,
            expanded: None,
            level: None,
            url: None,
            offscreen: false,
        },
        AxNode {
            id: "e2".to_string(),
            role: "textbox".to_string(),
            name: "Name".to_string(),
            value: Some(String::new()),
            depth: 1,
            bounds: Some(Rect::new(10.0, 80.0, 200.0, 24.0)),
            editable: true,
            focusable: true,
            disabled: false,
            checked: None,
            expanded: None,
            level: None,
            url: None,
            offscreen: false,
        },
    ]
}
