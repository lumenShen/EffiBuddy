//! The CDP backend: one browser engine, one active page session.
//!
//! Structure:
//!
//! - A [`CdpConnection`] to the browser-level WebSocket. Targets are attached
//!   with `Target.attachToTarget` (`flatten: true`), so page commands travel on
//!   the same socket with a `sessionId`.
//! - A background event task subscribes to `Runtime.consoleAPICalled`,
//!   `Log.entryAdded`, `Network.*` and `Page.javascriptDialogOpening`, feeding
//!   the [`Journal`] and auto-dismissing dialogs.
//! - Ref-based actions resolve `eN` through the most recent snapshot's own
//!   element records, so a click always targets the box the model saw.
//!
//! Everything the model can do funnels through here; policy lives in
//! [`crate::runtime`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::backend::{BrowserBackend, KeyChord, KeyModifier, WaitCondition, WaitRequest};
use crate::discover::{self, LaunchedBrowser};
use crate::journal::Journal;
use crate::keys::{self, KeySpec};
use crate::protocol::{CdpConnection, CdpEvent};
use crate::snapshot::{self, RawAxTree};
use crate::types::{
    AxNode, BrowserError, BrowserInfo, ConsoleEntry, ConsoleLevel, EngineKind, JournalPage,
    JournalQuery, MouseButton, NetEntry, PageCapture, PageFrame, PageInfo, Rect, ScreenshotOptions,
    Snapshot, SnapshotMode, TabInfo, TabInfoState, WaitPolicy,
};

const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 800;
/// Default raster cap when the caller does not ask for a specific size.
const DEFAULT_MAX_DIM: u32 = 1280;

/// One page row from `Target.getTargets`.
#[derive(Debug, Clone)]
struct TargetRow {
    id: String,
    url: String,
    title: String,
}

/// A page this backend has attached to.
struct PageSession {
    session_id: String,
    target_id: String,
}

struct Live {
    browser: LaunchedBrowser,
    connection: Arc<CdpConnection>,
    page: PageSession,
    /// Elements of the most recent snapshot: `e12` → node.
    refs: HashMap<String, AxNode>,
    /// Page-side stamp per ref (`data-effi-i`), which pins the exact element.
    stamps: HashMap<String, u32>,
    snapshot_url: String,
    snapshot_title: String,
    event_task: tokio::task::JoinHandle<()>,
    last_console_seq: u64,
}

pub struct CdpBackend {
    preference: Vec<EngineKind>,
    journal: Arc<Journal>,
    /// `None` until the browser is launched or attached.
    state: Mutex<Option<Live>>,
    /// Serializes snapshot creation so ref numbering cannot interleave.
    snapshot_lock: tokio::sync::Mutex<()>,
    headless: AtomicBool,
    /// Bumped on navigation so cached refs are known to be stale.
    generation: AtomicU64,
}

impl Drop for CdpBackend {
    fn drop(&mut self) {
        // Kill the browser we launched; leaving orphaned AI browsers behind is
        // exactly the mess this feature must avoid.
        if let Some(live) = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            live.event_task.abort();
            let mut browser = live.browser;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { browser.shutdown().await });
            } else if let Some(child) = browser.child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

impl CdpBackend {
    pub fn new(preference: Vec<EngineKind>) -> Self {
        Self {
            preference,
            journal: Arc::new(Journal::new(2000, 2000)),
            state: Mutex::new(None),
            snapshot_lock: tokio::sync::Mutex::new(()),
            headless: AtomicBool::new(true),
            generation: AtomicU64::new(0),
        }
    }

    pub fn preference(&self) -> &[EngineKind] {
        &self.preference
    }

    /// Switch between headless and windowed modes before connecting.
    pub fn set_headless(&self, headless: bool) {
        self.headless.store(headless, Ordering::SeqCst);
    }

    pub fn journal(&self) -> Arc<Journal> {
        self.journal.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Borrow the live session or explain that it does not exist yet.
    fn live<T>(
        &self,
        action: impl FnOnce(&Live) -> Result<T, BrowserError>,
    ) -> Result<T, BrowserError> {
        let guard = self.lock();
        match guard.as_ref() {
            Some(live) => action(live),
            None => Err(BrowserError::Launch(
                "browser is not connected yet; call ensure_ready first".to_string(),
            )),
        }
    }

    fn refresh_generation(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        let (connection, session_id) =
            self.live(|live| Ok((live.connection.clone(), live.page.session_id.clone())))?;
        connection.call_session(&session_id, method, params).await
    }

    async fn call_browser(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        let connection = self.live(|live| Ok(live.connection.clone()))?;
        connection.call(method, params).await
    }

    async fn page_targets(connection: &Arc<CdpConnection>) -> Result<Vec<TargetRow>, BrowserError> {
        let reply = connection.call("Target.getTargets", json!({})).await?;
        let infos = reply
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(infos
            .iter()
            .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|info| {
                Some(TargetRow {
                    id: info.get("targetId")?.as_str()?.to_string(),
                    url: info
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    title: info
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    /// Attach a page session and enable the domains the backend relies on.
    async fn attach_page(
        connection: &Arc<CdpConnection>,
        journal: Arc<Journal>,
        target_id: Option<String>,
    ) -> Result<(PageSession, tokio::task::JoinHandle<()>, u64), BrowserError> {
        let target_id = match target_id {
            Some(id) => id,
            None => Self::page_targets(connection)
                .await?
                .first()
                .map(|target| target.id.clone())
                .ok_or_else(|| {
                    BrowserError::Launch("browser has no page target to attach".to_string())
                })?,
        };

        let reply = connection
            .call(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        let session_id = reply
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("attachToTarget returned no sessionId".into()))?
            .to_string();

        // A failure here means we cannot observe the page, so surface it rather
        // than limping along with a half-enabled session.
        for method in [
            "Page.enable",
            "Runtime.enable",
            "Log.enable",
            "Network.enable",
            "DOM.enable",
        ] {
            connection
                .call_session(&session_id, method, json!({}))
                .await?;
        }

        let task = Self::spawn_event_pump(connection.clone(), journal, session_id.clone());
        Ok((
            PageSession {
                session_id,
                target_id,
            },
            task,
            0,
        ))
    }

    /// Feed console/network events into the journal and keep dialogs from
    /// blocking the session.
    fn spawn_event_pump(
        connection: Arc<CdpConnection>,
        journal: Arc<Journal>,
        session_id: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut events = connection.subscribe();
            // Status arrives on `responseReceived`, completion on
            // `loadingFinished`; buffer the status so the finished entry has it.
            let mut statuses: HashMap<String, u16> = HashMap::new();
            loop {
                let event: CdpEvent = match events.recv().await {
                    Ok(event) => event,
                    // A slow consumer dropping a burst of console lines is far
                    // better than blocking the page's event stream.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if event.session_id.as_deref() != Some(session_id.as_str()) {
                    continue;
                }
                match event.method.as_str() {
                    "Runtime.consoleAPICalled" => {
                        let (level, text) = console_entry_text(&event.params);
                        let url = event
                            .params
                            .get("stackTrace")
                            .and_then(|trace| trace.get("callFrames"))
                            .and_then(Value::as_array)
                            .and_then(|frames| frames.first())
                            .and_then(|frame| frame.get("url"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        journal.push_console(level, text, url, None);
                    }
                    "Log.entryAdded" => {
                        let entry = &event.params["entry"];
                        let level = match entry.get("level").and_then(Value::as_str) {
                            Some("error") => ConsoleLevel::Error,
                            Some("warning") => ConsoleLevel::Warning,
                            Some("verbose") => ConsoleLevel::Debug,
                            _ => ConsoleLevel::Info,
                        };
                        let text = entry
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let url = entry.get("url").and_then(Value::as_str).map(str::to_string);
                        let line = entry
                            .get("lineNumber")
                            .and_then(Value::as_u64)
                            .map(|line| line as u32);
                        journal.push_console(level, text, url, line);
                    }
                    "Network.requestWillBeSent" => {
                        let request_id = event.params["requestId"].as_str().unwrap_or_default();
                        let request = &event.params["request"];
                        journal.push_network_start(
                            request_id,
                            request
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or("GET"),
                            request
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            event
                                .params
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("other"),
                        );
                    }
                    "Network.responseReceived" => {
                        let request_id = event.params["requestId"].as_str().unwrap_or_default();
                        if let Some(status) = event.params["response"]
                            .get("status")
                            .and_then(Value::as_u64)
                            .map(|status| status as u16)
                        {
                            statuses.insert(request_id.to_string(), status);
                        }
                    }
                    "Network.loadingFinished" => {
                        let request_id = event.params["requestId"].as_str().unwrap_or_default();
                        let duration = event
                            .params
                            .get("timestamp")
                            .and_then(Value::as_f64)
                            .map(|_| 0u64);
                        let status = statuses.remove(request_id);
                        journal.push_network_finish(request_id, status, None, duration);
                    }
                    "Network.loadingFailed" => {
                        let request_id = event.params["requestId"].as_str().unwrap_or_default();
                        statuses.remove(request_id);
                        let failure = event
                            .params
                            .get("errorText")
                            .and_then(Value::as_str)
                            .unwrap_or("load failed")
                            .to_string();
                        journal.push_network_finish(request_id, None, Some(failure), None);
                    }
                    "Page.javascriptDialogOpening" => {
                        // Never let a modal block the session. Accepting is also
                        // what a user expects when the agent navigates away.
                        let _ = connection
                            .call_session(
                                &session_id,
                                "Page.handleJavaScriptDialog",
                                json!({"accept": true}),
                            )
                            .await;
                    }
                    _ => {}
                }
            }
        })
    }

    /// Read `readyState` + title + url in one round trip.
    async fn read_page(&self) -> Result<PageInfo, BrowserError> {
        let (target_id, cached_title, cached_url) = self.live(|live| {
            Ok((
                live.page.target_id.clone(),
                live.snapshot_title.clone(),
                live.snapshot_url.clone(),
            ))
        })?;
        let last_console = self.live(|live| Ok(live.last_console_seq))?;

        let reply = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": "({ready: document.readyState, url: location.href, title: document.title})",
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
            )
            .await;
        // A navigation in flight can make evaluate fail; fall back to cached
        // values instead of turning a transient into an action failure.
        // `Runtime.evaluate` nests the by-value payload twice:
        // `{result: {type, value: {...}}}`.
        let result = reply
            .ok()
            .and_then(|value| value.get("result").cloned())
            .and_then(|result| result.get("value").cloned())
            .unwrap_or(Value::Null);

        Ok(PageInfo {
            tab_id: target_id,
            url: result
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(cached_url),
            title: result
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(cached_title),
            ready_state: result
                .get("ready")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            console_errors: self.journal.console_errors_since(Some(last_console)),
            pending_requests: 0,
        })
    }

    /// Wait for the page to settle after an action or navigation.
    async fn settle(&self, policy: WaitPolicy) -> Result<PageInfo, BrowserError> {
        match policy {
            WaitPolicy::None => self.read_page().await,
            WaitPolicy::DomContentLoaded => {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    let info = self.read_page().await?;
                    if info.ready_state != "loading" || Instant::now() >= deadline {
                        return Ok(info);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            WaitPolicy::Load => {
                // Wait for `complete`, then hold for a quiet window so SPAs have
                // painted something a snapshot can describe.
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut info = self.read_page().await?;
                while info.ready_state != "complete" && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    info = self.read_page().await?;
                }
                let quiet_deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < quiet_deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    info = self.read_page().await?;
                }
                Ok(info)
            }
        }
    }

    async fn collect_snapshot(
        &self,
        mode: SnapshotMode,
        max_nodes: usize,
    ) -> Result<(Snapshot, Vec<(String, u32)>), BrowserError> {
        let value = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": snapshot::AX_TREE_JS,
                    "returnByValue": true,
                    "awaitPromise": false,
                    "timeout": 10000,
                }),
            )
            .await?;

        let text = value
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                let detail = value
                    .get("exceptionDetails")
                    .map(|details| details.to_string())
                    .unwrap_or_else(|| value.to_string());
                BrowserError::Page(format!("snapshot script failed: {detail}"))
            })?;

        let raw: RawAxTree = serde_json::from_str(text)
            .map_err(|error| BrowserError::Page(format!("snapshot JSON parse failed: {error}")))?;
        let id = format!("n{}", self.generation.load(Ordering::Relaxed) + 1);
        Ok(snapshot::build_snapshot_with_stamps(
            &raw, mode, max_nodes, id,
        ))
    }

    /// Page stamp for a ref, or a stale-ref error.
    fn stamp_of(&self, node: &AxNode) -> Result<u32, BrowserError> {
        self.live(|live| Ok(live.stamps.get(&node.id).copied()))?
            .ok_or_else(|| {
                BrowserError::NotInteractable(format!(
                    "{}「{}」has no page stamp; take a new browser_snapshot",
                    node.role, node.name
                ))
            })
    }

    /// The element a ref pointed at in the most recent snapshot.
    fn lookup_ref(&self, r#ref: &str) -> Result<AxNode, BrowserError> {
        self.live(|live| {
            live.refs.get(r#ref).cloned().ok_or_else(|| {
                BrowserError::NotFound(format!(
                    "ref `{}` is not in the current snapshot; take a new browser_snapshot",
                    r#ref
                ))
            })
        })
    }

    /// Scroll an element into view and return its current box.
    ///
    /// The snapshot script stamps each collected element with
    /// `data-effi-i="<n>"`, so a ref resolves to exactly the element the model
    /// saw — no guessing by text or geometry. A missing stamp means the page
    /// re-rendered since the snapshot, which is precisely when the model must
    /// take a fresh snapshot instead of clicking blind.
    async fn scroll_into_view(&self, node: &AxNode) -> Result<Rect, BrowserError> {
        let stamp = self.stamp_of(node)?;
        let script = format!(
            r#"(() => {{
                const el = document.querySelector('[data-effi-i="' + {stamp} + '"]');
                if (!el || !el.isConnected) return null;
                el.scrollIntoView({{ block: 'center', inline: 'center' }});
                const box = el.getBoundingClientRect();
                if (box.width <= 0 || box.height <= 0) return null;
                return {{ x: box.x, y: box.y, width: box.width, height: box.height }};
            }})()"#,
            stamp = stamp,
        );
        let value = self.eval_value(&script).await?;
        if value.is_null() {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is no longer visible on the page; take a new browser_snapshot",
                node.role, node.name
            )));
        }
        Ok(Rect::new(
            value.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            value.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            value.get("width").and_then(Value::as_f64).unwrap_or(0.0),
            value.get("height").and_then(Value::as_f64).unwrap_or(0.0),
        ))
    }

    async fn mouse_click_at(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        modifiers: &[KeyModifier],
    ) -> Result<(), BrowserError> {
        let mask = modifiers
            .iter()
            .fold(0, |mask, modifier| mask | modifier.cdp_mask());
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "modifiers": mask}),
        )
        .await?;
        for count in 1..=click_count.max(1) {
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": x,
                    "y": y,
                    "button": button.cdp_button(),
                    "buttons": button.cdp_mask(),
                    "clickCount": count,
                    "modifiers": mask,
                }),
            )
            .await?;
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": x,
                    "y": y,
                    "button": button.cdp_button(),
                    "buttons": 0,
                    "clickCount": count,
                    "modifiers": mask,
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn send_key_event(
        &self,
        event_type: &str,
        spec: &KeySpec,
        modifiers: i32,
        text: Option<String>,
    ) -> Result<(), BrowserError> {
        let mut params = json!({
            "type": event_type,
            "key": spec.key,
            "code": spec.code,
            "windowsVirtualKeyCode": spec.vk,
            "nativeVirtualKeyCode": spec.vk,
            "modifiers": modifiers,
            "location": spec.location,
        });
        if let Some(text) = text {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        self.call("Input.dispatchKeyEvent", params).await?;
        Ok(())
    }

    /// Dispatch a full key press: modifiers down, key down/up, modifiers up.
    async fn dispatch_key(&self, chord: &KeyChord) -> Result<(), BrowserError> {
        let mask = keys::modifier_mask(chord);
        let modifier_specs = keys::modifier_specs(&chord.modifiers);
        let spec = keys::key_spec(chord).map_err(BrowserError::Invalid)?;

        for modifier in &modifier_specs {
            self.send_key_event("rawKeyDown", modifier, mask, None)
                .await?;
        }
        // Printable keys must go out as `keyDown` with `text`, or text fields
        // receive nothing.
        let printable = spec
            .text
            .clone()
            .filter(|text| !text.is_empty())
            .filter(|_| !chord.has(KeyModifier::Ctrl) && !chord.has(KeyModifier::Super));
        match printable {
            Some(text) => {
                self.send_key_event("keyDown", &spec, mask, Some(text))
                    .await?;
            }
            None => {
                self.send_key_event("keyDown", &spec, mask, None).await?;
            }
        }
        self.send_key_event("keyUp", &spec, mask, None).await?;
        for modifier in modifier_specs.iter().rev() {
            self.send_key_event("keyUp", modifier, mask, None).await?;
        }
        Ok(())
    }

    /// Insert text into the focused element without synthesizing per-character
    /// key events (Unicode-safe and much faster for long input).
    async fn insert_text(&self, text: &str) -> Result<(), BrowserError> {
        self.call("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    async fn focus_ref(&self, r#ref: &str) -> Result<(), BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        let bounds = self.scroll_into_view(&node).await?;
        let (x, y) = bounds.center();
        // Focusing alone does not place a caret in contenteditable or select-all
        // inputs, so a real click is the reliable path.
        self.mouse_click_at(x, y, MouseButton::Left, 1, &[]).await
    }

    /// Evaluate an expression and return its by-value result.
    async fn eval_value(&self, expression: &str) -> Result<Value, BrowserError> {
        let reply = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(details) = reply.get("exceptionDetails") {
            let message = details
                .get("exception")
                .and_then(|exception| exception.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation failed");
            return Err(BrowserError::Page(message.to_string()));
        }
        Ok(reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Apply the standard viewport override used for screenshots and layout.
    async fn apply_viewport(&self) -> Result<(), BrowserError> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": DEFAULT_VIEWPORT_WIDTH,
                "height": DEFAULT_VIEWPORT_HEIGHT,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )
        .await?;
        Ok(())
    }
}

/// Build the console text for a `Runtime.consoleAPICalled` event.
fn console_entry_text(params: &Value) -> (ConsoleLevel, String) {
    let level = match params.get("type").and_then(Value::as_str) {
        Some("error") | Some("assert") => ConsoleLevel::Error,
        Some("warning") => ConsoleLevel::Warning,
        Some("debug") => ConsoleLevel::Debug,
        Some("info") => ConsoleLevel::Info,
        _ => ConsoleLevel::Log,
    };
    let mut parts = Vec::new();
    if let Some(args) = params.get("args").and_then(Value::as_array) {
        for argument in args {
            let text = argument
                .get("value")
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .or_else(|| {
                    argument
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| argument.to_string());
            parts.push(text);
        }
    }
    (level, parts.join(" "))
}

/// Downscale a PNG so it fits the model's attachment budget.
fn shrink_png(png: &[u8], max_dim: u32) -> Result<(Vec<u8>, u32, u32), BrowserError> {
    let image = image::load_from_memory(png)
        .map_err(|error| BrowserError::Page(format!("screenshot decode failed: {error}")))?;
    let (width, height) = (image.width(), image.height());
    if width <= max_dim && height <= max_dim {
        return Ok((png.to_vec(), width, height));
    }
    let scale = max_dim as f64 / width.max(height) as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);
    let resized = image.resize(new_width, new_height, image::imageops::FilterType::Triangle);
    let mut buffer = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|error| BrowserError::Page(format!("screenshot encode failed: {error}")))?;
    Ok((buffer.into_inner(), new_width, new_height))
}

#[async_trait]
impl BrowserBackend for CdpBackend {
    fn info(&self) -> BrowserInfo {
        let headless = self.headless.load(Ordering::SeqCst);
        let guard = self.lock();
        match guard.as_ref() {
            Some(live) => BrowserInfo {
                engine: Some(live.browser.engine.kind),
                executable: Some(live.browser.engine.path.clone()),
                version: live
                    .browser
                    .notes
                    .iter()
                    .find_map(|note| note.strip_prefix("browser: ").map(str::to_string)),
                connected: !live.connection.is_closed(),
                headless,
                debug_url: Some(live.browser.debug_url.clone()),
                profile_dir: Some(live.browser.profile_dir.clone()),
                notes: live.browser.notes.clone(),
            },
            None => BrowserInfo {
                headless,
                notes: vec!["尚未启动浏览器；首次使用时会自动启动或连接。".to_string()],
                ..BrowserInfo::default()
            },
        }
    }

    async fn ensure_ready(&self) -> Result<PageInfo, BrowserError> {
        // Fast path: already attached to a live socket.
        let already_live = {
            let guard = self.lock();
            guard
                .as_ref()
                .map(|live| !live.connection.is_closed())
                .unwrap_or(false)
        };
        if already_live {
            return self.read_page().await;
        }

        let headless = self.headless.load(Ordering::SeqCst);
        let browser = discover::launch(&self.preference, headless).await?;
        let connection = CdpConnection::connect(&browser.debug_url).await?;
        let (page, task, last_console) =
            Self::attach_page(&connection, self.journal.clone(), None).await?;

        {
            let mut guard = self.lock();
            if let Some(old) = guard.take() {
                old.event_task.abort();
                let mut previous = old.browser;
                tokio::spawn(async move { previous.shutdown().await });
            }
            *guard = Some(Live {
                browser,
                connection,
                page,
                refs: HashMap::new(),
                stamps: HashMap::new(),
                snapshot_url: String::new(),
                snapshot_title: String::new(),
                event_task: task,
                last_console_seq: last_console,
            });
        }

        // Headless defaults to 800x600, which makes layout and screenshots
        // unnecessarily cramped.
        let _ = self.apply_viewport().await;
        self.read_page().await
    }

    async fn end_session(&self) -> Result<(), BrowserError> {
        let live = {
            let mut guard = self.lock();
            guard.take()
        };
        let Some(mut live) = live else {
            return Ok(());
        };
        live.event_task.abort();
        // We always launch our own browser with our own profile, so closing it
        // is correct; a user's own browser is never taken over silently.
        let _ = live.connection.call("Browser.close", json!({})).await;
        live.browser.shutdown().await;
        self.journal.clear();
        self.refresh_generation();
        Ok(())
    }

    async fn navigate(&self, url: &str, wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await?;
        self.call("Page.navigate", json!({ "url": url })).await?;
        self.refresh_generation();
        self.settle(wait).await
    }

    async fn reload(&self, wait: WaitPolicy) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await?;
        self.call("Page.reload", json!({"ignoreCache": false}))
            .await?;
        self.refresh_generation();
        self.settle(wait).await
    }

    async fn history(&self, back: bool) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await?;
        let expression = if back {
            "history.back(); 'ok'"
        } else {
            "history.forward(); 'ok'"
        };
        self.eval_value(expression).await?;
        self.refresh_generation();
        self.settle(WaitPolicy::Load).await
    }

    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
        self.ensure_ready().await?;
        let (connection, active) =
            self.live(|live| Ok((live.connection.clone(), live.page.target_id.clone())))?;
        let rows = Self::page_targets(&connection).await?;
        Ok(rows
            .into_iter()
            .map(|row| TabInfo {
                state: if row.url == "about:blank" {
                    TabInfoState::Loading
                } else {
                    TabInfoState::Ready
                },
                active: row.id == active,
                id: row.id,
                url: row.url,
                title: row.title,
            })
            .collect())
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<TabInfo, BrowserError> {
        self.ensure_ready().await?;
        let target = url.unwrap_or("about:blank");
        let reply = self
            .call_browser("Target.createTarget", json!({ "url": target }))
            .await?;
        let id = reply
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("createTarget returned no targetId".into()))?
            .to_string();
        Ok(TabInfo {
            id,
            url: target.to_string(),
            title: String::new(),
            state: TabInfoState::Ready,
            active: true,
        })
    }

    async fn select_tab(&self, tab_id: &str) -> Result<PageInfo, BrowserError> {
        self.ensure_ready().await?;
        let (connection, old_session) =
            self.live(|live| Ok((live.connection.clone(), live.page.session_id.clone())))?;
        // Detach the previous session so only one event stream feeds the journal.
        let _ = connection
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": old_session }),
            )
            .await;

        let (page, task, last_console) =
            Self::attach_page(&connection, self.journal.clone(), Some(tab_id.to_string())).await?;
        {
            let mut guard = self.lock();
            if let Some(live) = guard.as_mut() {
                live.event_task.abort();
                live.page = page;
                live.event_task = task;
                live.refs.clear();
                live.stamps.clear();
                live.last_console_seq = last_console;
            }
        }
        let _ = self.apply_viewport().await;
        let _ = self.call("Page.bringToFront", json!({})).await;
        self.refresh_generation();
        self.read_page().await
    }

    async fn close_tab(&self, tab_id: &str) -> Result<(), BrowserError> {
        self.ensure_ready().await?;
        let active = self.live(|live| Ok(live.page.target_id.clone()))?;
        if active == tab_id {
            return Err(BrowserError::Invalid(
                "cannot close the tab the session is attached to; select another tab first"
                    .to_string(),
            ));
        }
        self.call_browser("Target.closeTarget", json!({ "targetId": tab_id }))
            .await?;
        Ok(())
    }

    async fn snapshot(
        &self,
        mode: SnapshotMode,
        max_nodes: usize,
    ) -> Result<Snapshot, BrowserError> {
        self.ensure_ready().await?;
        let _guard = self.snapshot_lock.lock().await;
        let (snapshot, stamps) = self.collect_snapshot(mode, max_nodes).await?;
        {
            let mut guard = self.lock();
            if let Some(live) = guard.as_mut() {
                live.refs = snapshot
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), node.clone()))
                    .collect();
                live.stamps = stamps.into_iter().collect();
                live.snapshot_url = snapshot.url.clone();
                live.snapshot_title = snapshot.title.clone();
            }
        }
        Ok(snapshot)
    }

    async fn page_text(&self, max_chars: usize) -> Result<String, BrowserError> {
        self.ensure_ready().await?;
        let value = self.eval_value(snapshot::PAGE_TEXT_JS).await?;
        let text = value.as_str().unwrap_or_default().to_string();
        if text.chars().count() <= max_chars {
            return Ok(text);
        }
        let mut out: String = text.chars().take(max_chars).collect();
        out.push('…');
        Ok(out)
    }

    async fn screenshot(&self, options: ScreenshotOptions) -> Result<PageCapture, BrowserError> {
        self.ensure_ready().await?;
        let mut params = json!({ "format": "png", "captureBeyondViewport": false });
        if options.full_page {
            let metrics = self
                .call("Page.getLayoutMetrics", json!({}))
                .await
                .unwrap_or(Value::Null);
            let width = metrics
                .get("cssContentSize")
                .and_then(|size| size.get("width"))
                .and_then(Value::as_f64)
                .unwrap_or(DEFAULT_VIEWPORT_WIDTH as f64);
            let height = metrics
                .get("cssContentSize")
                .and_then(|size| size.get("height"))
                .and_then(Value::as_f64)
                .unwrap_or(DEFAULT_VIEWPORT_HEIGHT as f64);
            params["captureBeyondViewport"] = json!(true);
            params["clip"] = json!({
                "x": 0, "y": 0, "width": width, "height": height, "scale": 1,
            });
        }
        let reply = self.call("Page.captureScreenshot", params).await?;
        let encoded = reply
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("captureScreenshot returned no data".into()))?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| BrowserError::Protocol(format!("bad screenshot base64: {error}")))?;

        let (png, image_width, image_height) =
            shrink_png(&png, options.max_dim.unwrap_or(DEFAULT_MAX_DIM))?;

        // `native_*` is the CSS viewport, so a model pixel can be mapped back to
        // CSS pixels for `click` by hand when it prefers coordinates to refs.
        let metrics = self
            .call("Page.getLayoutMetrics", json!({}))
            .await
            .unwrap_or(Value::Null);
        let native_width = metrics
            .get("cssLayoutViewport")
            .and_then(|value| value.get("clientWidth"))
            .and_then(Value::as_f64)
            .map(|value| value as u32)
            .unwrap_or(image_width);
        let native_height = metrics
            .get("cssLayoutViewport")
            .and_then(|value| value.get("clientHeight"))
            .and_then(Value::as_f64)
            .map(|value| value as u32)
            .unwrap_or(image_height);

        Ok(PageCapture {
            png,
            frame: PageFrame {
                id: String::new(),
                image_width,
                image_height,
                native_width,
                native_height,
            },
        })
    }

    async fn click_ref(
        &self,
        r#ref: &str,
        button: MouseButton,
        click_count: u32,
        modifiers: &[KeyModifier],
        wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        if node.disabled {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is disabled",
                node.role, node.name
            )));
        }
        let bounds = self.scroll_into_view(&node).await?;
        if !bounds.is_visible() {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」has no layout box",
                node.role, node.name
            )));
        }
        let (x, y) = bounds.center();
        self.mouse_click_at(x, y, button, click_count, modifiers)
            .await?;
        // A click often navigates or re-renders; refs from the old snapshot are
        // no longer safe to act on.
        self.refresh_generation();
        self.settle(wait).await
    }

    async fn click_frame_pixel(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        _wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        // A pixel click has no element identity to verify, so there is nothing
        // to wait on beyond the page settling on its own.
        self.mouse_click_at(x, y, button, click_count, &[]).await?;
        self.refresh_generation();
        self.settle(WaitPolicy::None).await
    }

    async fn hover_ref(&self, r#ref: &str) -> Result<PageInfo, BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        let bounds = self.scroll_into_view(&node).await?;
        let (x, y) = bounds.center();
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "buttons": 0}),
        )
        .await?;
        self.read_page().await
    }

    async fn type_ref(
        &self,
        r#ref: &str,
        text: &str,
        submit: bool,
        wait: WaitPolicy,
    ) -> Result<PageInfo, BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        if node.disabled {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is disabled",
                node.role, node.name
            )));
        }
        // Read-only inputs accept focus but silently drop inserted text, which
        // looks like success to the model. Fail loudly instead.
        let writable = node.editable
            || matches!(
                node.role.as_str(),
                "textbox" | "searchbox" | "combobox" | "spinbutton"
            );
        if !writable {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is not editable",
                node.role, node.name
            )));
        }

        self.focus_ref(r#ref).await?;
        // Select-all then insert replaces the existing value, matching how a user
        // overwrites a field instead of appending to it.
        self.dispatch_key(&KeyChord {
            modifiers: vec![KeyModifier::Ctrl],
            key: "a".to_string(),
        })
        .await?;
        if !text.is_empty() {
            self.insert_text(text).await?;
        }
        if submit {
            self.dispatch_key(&KeyChord {
                modifiers: Vec::new(),
                key: "enter".to_string(),
            })
            .await?;
            self.refresh_generation();
            // Submitting normally navigates, so honour the caller's policy
            // instead of forcing a full load wait on every search box.
            let policy = if wait == WaitPolicy::None {
                WaitPolicy::DomContentLoaded
            } else {
                wait
            };
            return self.settle(policy).await;
        }
        self.read_page().await
    }

    async fn type_text(&self, text: &str) -> Result<PageInfo, BrowserError> {
        self.insert_text(text).await?;
        self.read_page().await
    }

    async fn press_key(&self, chord: &KeyChord) -> Result<PageInfo, BrowserError> {
        self.dispatch_key(chord).await?;
        self.settle(WaitPolicy::None).await
    }

    async fn scroll(
        &self,
        r#ref: Option<&str>,
        dx: i32,
        dy: i32,
    ) -> Result<PageInfo, BrowserError> {
        let (x, y) = match r#ref {
            Some(r#ref) => {
                let node = self.lookup_ref(r#ref)?;
                self.scroll_into_view(&node).await?.center()
            }
            None => (
                DEFAULT_VIEWPORT_WIDTH as f64 / 2.0,
                DEFAULT_VIEWPORT_HEIGHT as f64 / 2.0,
            ),
        };
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": dx * 100,
                "deltaY": dy * 100,
                "button": "none",
            }),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        self.read_page().await
    }

    async fn select_option(&self, r#ref: &str, value: &str) -> Result<PageInfo, BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        let stamp = self.stamp_of(&node)?;
        let script = format!(
            r#"(() => {{
                const select = document.querySelector('[data-effi-i="' + {stamp} + '"]');
                if (!select || !select.isConnected || select.tagName !== 'SELECT') return null;
                const wanted = {value};
                const option = Array.from(select.options).find(
                    (item) => item.value === wanted || (item.text || '').trim() === wanted
                );
                if (!option) return {{ missing: true, options: Array.from(select.options).map((item) => item.value) }};
                select.value = option.value;
                select.dispatchEvent(new Event('input', {{ bubbles: true }}));
                select.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return {{ value: option.value }};
            }})()"#,
            stamp = stamp,
            value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
        );
        let picked = self.eval_value(&script).await?;
        if picked.get("missing").and_then(Value::as_bool) == Some(true) {
            return Err(BrowserError::NotFound(format!(
                "no option matching `{value}`; available: {}",
                picked.get("options").cloned().unwrap_or(Value::Null)
            )));
        }
        if picked.is_null() {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is not a <select>; take a new browser_snapshot",
                node.role, node.name
            )));
        }
        self.refresh_generation();
        self.read_page().await
    }

    async fn set_checked(&self, r#ref: &str, checked: bool) -> Result<PageInfo, BrowserError> {
        let node = self.lookup_ref(r#ref)?;
        if !matches!(
            node.role.as_str(),
            "checkbox" | "radio" | "switch" | "menuitemcheckbox"
        ) {
            return Err(BrowserError::NotInteractable(format!(
                "{}「{}」is not a checkbox or radio",
                node.role, node.name
            )));
        }
        let stamp = self.stamp_of(&node)?;
        let script = format!(
            r#"(() => {{
                const el = document.querySelector('[data-effi-i="' + {stamp} + '"]');
                if (!el || !el.isConnected) return null;
                const target = {checked};
                if (el.checked === target) return {{ changed: false }};
                el.click();
                return {{ changed: true, checked: el.checked }};
            }})()"#,
            stamp = stamp,
            checked = if checked { "true" } else { "false" },
        );
        let result = self.eval_value(&script).await?;
        if result.is_null() {
            return Err(BrowserError::NotFound(format!(
                "checkbox「{}」is no longer on the page; take a new browser_snapshot",
                node.name
            )));
        }
        self.refresh_generation();
        self.read_page().await
    }

    async fn upload_file(
        &self,
        r#ref: &str,
        path: &std::path::Path,
    ) -> Result<PageInfo, BrowserError> {
        let _ = self.lookup_ref(r#ref)?;
        // `DOM.setFileInputFiles` needs a nodeId, so resolve the file input
        // through the document instead of relying on CDP's chooser intercept.
        let document = self.call("DOM.getDocument", json!({"depth": -1})).await?;
        let root_id = document
            .get("root")
            .and_then(|root| root.get("nodeId"))
            .and_then(Value::as_i64)
            .ok_or_else(|| BrowserError::Protocol("DOM.getDocument returned no root".into()))?;
        let query = self
            .call(
                "DOM.querySelector",
                json!({"nodeId": root_id, "selector": "input[type=file]"}),
            )
            .await?;
        let node_id = query.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
        if node_id == 0 {
            return Err(BrowserError::NotFound(
                "this page has no <input type=file>".to_string(),
            ));
        }
        let files = vec![path.to_string_lossy().to_string()];
        self.call(
            "DOM.setFileInputFiles",
            json!({ "nodeId": node_id, "files": files }),
        )
        .await?;
        self.refresh_generation();
        self.settle(WaitPolicy::Load).await
    }

    async fn evaluate(&self, expression: &str) -> Result<Value, BrowserError> {
        self.ensure_ready().await?;
        self.eval_value(expression).await
    }

    async fn console(
        &self,
        query: &JournalQuery,
    ) -> Result<JournalPage<ConsoleEntry>, BrowserError> {
        Ok(self.journal.console(query))
    }

    async fn network(&self, query: &JournalQuery) -> Result<JournalPage<NetEntry>, BrowserError> {
        Ok(self.journal.network(query))
    }

    async fn wait_for(&self, request: &WaitRequest) -> Result<PageInfo, BrowserError> {
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms.max(100));
        let argument = request.argument.clone().unwrap_or_default();
        loop {
            match request.condition {
                WaitCondition::Settled => {
                    let info = self.settle(WaitPolicy::Load).await?;
                    if info.ready_state == "complete" {
                        return Ok(info);
                    }
                }
                WaitCondition::UrlContains => {
                    let info = self.read_page().await?;
                    if info.url.contains(&argument) {
                        return Ok(info);
                    }
                }
                WaitCondition::RefVisible => {
                    let visible = self
                        .lookup_ref(&argument)
                        .ok()
                        .and_then(|node| node.bounds)
                        .map(|bounds| bounds.is_visible())
                        .unwrap_or(false);
                    if visible {
                        return self.read_page().await;
                    }
                }
                WaitCondition::SelectorVisible => {
                    let expression = format!(
                        r#"(() => {{
                            const el = document.querySelector({selector});
                            if (!el) return false;
                            const box = el.getBoundingClientRect();
                            return box.width > 0 && box.height > 0;
                        }})()"#,
                        selector =
                            serde_json::to_string(&argument).unwrap_or_else(|_| "\"\"".into()),
                    );
                    if let Ok(value) = self.eval_value(&expression).await
                        && value.as_bool() == Some(true)
                    {
                        return self.read_page().await;
                    }
                }
                WaitCondition::ConsoleError => {
                    if self.journal.console_errors_since(request.since) > 0 {
                        return self.read_page().await;
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!(
                    "wait_for did not settle within {}ms",
                    request.timeout_ms
                )));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    async fn page_info(&self) -> Result<PageInfo, BrowserError> {
        self.read_page().await
    }

    async fn ref_bounds(&self, r#ref: &str) -> Result<Option<Rect>, BrowserError> {
        Ok(self.lookup_ref(r#ref)?.bounds)
    }

    async fn ref_node(&self, r#ref: &str) -> Result<Option<AxNode>, BrowserError> {
        Ok(self.live(|live| Ok(live.refs.get(r#ref).cloned()))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_text_prefers_values_then_descriptions() {
        let params = json!({
            "type": "error",
            "args": [
                {"value": "boom"},
                {"description": "TypeError: x is undefined"},
                {"type": "object"}
            ]
        });
        let (level, text) = console_entry_text(&params);
        assert_eq!(level, ConsoleLevel::Error);
        assert!(text.starts_with("boom"), "{text}");
        assert!(text.contains("TypeError"), "{text}");
    }

    #[test]
    fn console_levels_map_from_cdp_types() {
        for (cdp, expected) in [
            ("error", ConsoleLevel::Error),
            ("assert", ConsoleLevel::Error),
            ("warning", ConsoleLevel::Warning),
            ("debug", ConsoleLevel::Debug),
            ("info", ConsoleLevel::Info),
            ("log", ConsoleLevel::Log),
        ] {
            let (level, _) = console_entry_text(&json!({"type": cdp, "args": []}));
            assert_eq!(level, expected, "{cdp}");
        }
    }

    #[test]
    fn shrink_png_leaves_small_images_untouched() {
        let image = image::RgbaImage::from_pixel(40, 20, image::Rgba([1, 2, 3, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let bytes = png.into_inner();
        let (out, width, height) = shrink_png(&bytes, 1280).unwrap();
        assert_eq!((width, height), (40, 20));
        assert_eq!(out, bytes);
    }

    #[test]
    fn shrink_png_scales_the_longest_edge() {
        let image = image::RgbaImage::from_pixel(2000, 1000, image::Rgba([9, 9, 9, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (_, width, height) = shrink_png(&png.into_inner(), 500).unwrap();
        assert_eq!(width, 500);
        assert_eq!(height, 250);
    }

    #[test]
    fn backend_reports_not_connected_before_launch() {
        let backend = CdpBackend::new(vec![EngineKind::Chrome]);
        let info = backend.info();
        assert!(!info.connected);
        assert_eq!(info.engine, None);
    }
}
