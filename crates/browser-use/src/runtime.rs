//! Snapshot bookkeeping, policy, pause/stop and action orchestration.
//!
//! The backend is stateless with respect to the agent. Every decision about who
//! may drive the browser, which snapshot a `ref` belongs to, whether a URL is
//! allowed and whether the user has stopped the session lives here.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::backend::{BrowserBackend, KeyChord};
use crate::types::{
    AxNode, BrowserError, BrowserInfo, BrowserUseMode, ConsoleEntry, EditorOp, EditorRequest,
    ElementText, EvalOutcome, JournalPage, JournalQuery, MouseButton, NetEntry, PageCapture,
    PageFrame, PageInfo, Rect, RequestWait, ResponseBody, ScreenshotOptions, Snapshot,
    SnapshotMode, TabInfo, WaitPolicy,
};

/// How many snapshots stay addressable. A model typically batches a few actions
/// per snapshot; older refs must fail closed.
pub const MAX_SNAPSHOTS: usize = 4;

/// How many page rasters stay addressable for pixel clicks.
pub const MAX_FRAMES: usize = 4;

pub type OverlayEmitter = Arc<dyn Fn(OverlayEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
pub struct OverlayEvent {
    pub session_id: String,
    #[serde(flatten)]
    pub kind: OverlayEventKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayEventKind {
    /// Open the bar if needed and show this status.
    Show { message: String, paused: bool },
    /// The bar is already open; update its status.
    Update { message: String, paused: bool },
    /// Hide the bar (session ended or stopped).
    Hide,
}

/// Query for the console/network journals. Alias of [`JournalQuery`] so the
/// tool adapter and the backend share one type.
pub type ConsoleQuery = JournalQuery;

struct SessionRuntime {
    /// Browser tab this conversation drives. One browser process is shared by
    /// the whole agent tree, so every conversation owns a tab and the runtime
    /// switches back to it before acting.
    tab_id: Option<String>,
    paused: bool,
    stopped: bool,
    notify: Arc<Notify>,
    snapshots: VecDeque<Snapshot>,
    frames: VecDeque<PageFrame>,
    last_url: String,
    last_title: String,
    overlay_visible: bool,
    last_message: String,
    console_cursor: u64,
    /// Scripts injected into every new document of this conversation's tab.
    init_scripts: Vec<String>,
}

impl SessionRuntime {
    fn new() -> Self {
        Self {
            tab_id: None,
            paused: false,
            stopped: false,
            notify: Arc::new(Notify::new()),
            snapshots: VecDeque::new(),
            frames: VecDeque::new(),
            last_url: String::new(),
            last_title: String::new(),
            overlay_visible: false,
            last_message: "正在连接浏览器…".to_string(),
            console_cursor: 0,
            init_scripts: Vec::new(),
        }
    }

    fn record_snapshot(&mut self, snapshot: Snapshot) -> String {
        let id = snapshot.id.clone();
        self.snapshots.push_back(snapshot);
        while self.snapshots.len() > MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
        id
    }

    fn snapshot(&self, id: Option<&str>) -> Result<&Snapshot, BrowserError> {
        match id {
            Some(id) if !id.trim().is_empty() => self
                .snapshots
                .iter()
                .rev()
                .find(|snapshot| snapshot.id == id)
                .ok_or_else(|| {
                    BrowserError::StaleSnapshot(format!(
                        "snapshot `{id}` is no longer available; take a new browser_snapshot"
                    ))
                }),
            _ => self.snapshots.back().ok_or_else(|| {
                BrowserError::StaleSnapshot("no snapshot has been taken yet".into())
            }),
        }
    }

    fn record_frame(&mut self, mut frame: PageFrame) -> PageFrame {
        if frame.id.is_empty() {
            frame.id = format!("s{}", self.frames.len() + 1);
        }
        self.frames.push_back(frame.clone());
        while self.frames.len() > MAX_FRAMES {
            self.frames.pop_front();
        }
        frame
    }

    fn frame(&self, id: Option<&str>) -> Result<&PageFrame, BrowserError> {
        match id {
            Some(id) if !id.trim().is_empty() => self
                .frames
                .iter()
                .rev()
                .find(|frame| frame.id == id)
                .ok_or_else(|| {
                    BrowserError::StaleFrame(format!(
                        "frame `{id}` is no longer available; take a new browser_screenshot"
                    ))
                }),
            _ => self
                .frames
                .back()
                .ok_or_else(|| BrowserError::StaleFrame("no screenshot has been taken yet".into())),
        }
    }

    fn note_page(&mut self, info: &PageInfo) {
        if !info.url.is_empty() {
            self.last_url = info.url.clone();
        }
        if !info.title.is_empty() {
            self.last_title = info.title.clone();
        }
    }
}

pub struct BrowserRuntime {
    /// Tab the backend is currently attached to. Only this runtime switches
    /// tabs, and every action result carries the real `tab_id`, so this stays
    /// authoritative without an extra CDP round trip per call.
    active_tab: Mutex<Option<String>>,
    mode: RwLock<BrowserUseMode>,
    backend: Arc<dyn BrowserBackend>,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    overlay: RwLock<Option<OverlayEmitter>>,
    data_dir: PathBuf,
    snapshot_counter: AtomicU64,
    frame_counter: AtomicU64,
    /// When true, `file://` and other local-scheme URLs are allowed.
    allow_local_files: bool,
}

impl BrowserRuntime {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self::with_data_dir(backend, crate::discover::data_dir())
    }

    pub fn with_data_dir(backend: Arc<dyn BrowserBackend>, data_dir: PathBuf) -> Self {
        let allow_local_files = std::env::var("EFFIBUDDY_BROWSER_ALLOW_FILE")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            active_tab: Mutex::new(None),
            mode: RwLock::new(BrowserUseMode::default()),
            backend,
            sessions: Mutex::new(HashMap::new()),
            overlay: RwLock::new(None),
            data_dir,
            snapshot_counter: AtomicU64::new(0),
            frame_counter: AtomicU64::new(0),
            allow_local_files,
        }
    }

    pub fn set_mode(&self, mode: BrowserUseMode) {
        *self
            .mode
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
    }

    pub fn mode(&self) -> BrowserUseMode {
        *self
            .mode
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_allow_local_files(&mut self, allow: bool) {
        self.allow_local_files = allow;
    }

    pub fn set_overlay(&self, emitter: OverlayEmitter) {
        *self
            .overlay
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(emitter);
    }

    pub fn info(&self) -> BrowserInfo {
        self.backend.info()
    }

    pub fn backend(&self) -> Arc<dyn BrowserBackend> {
        self.backend.clone()
    }

    /// Release anything the session was holding and stop the drive. Approval
    /// lives in the tool permission system, so nothing is revoked here.
    pub fn stop(&self, session_id: &str) {
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = sessions.get_mut(session_id) {
                session.stopped = true;
                session.paused = false;
                session.snapshots.clear();
                session.frames.clear();
                session.notify.notify_waiters();
            }
        }
        self.emit(session_id, OverlayEventKind::Hide);
    }

    pub fn is_stopped(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|session| session.stopped)
            .unwrap_or(false)
    }

    /// Pause before the next action so the user can take over.
    pub fn pause(&self, session_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.paused = true;
        }
    }

    pub fn resume(&self, session_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.paused = false;
            session.notify.notify_waiters();
        }
    }

    pub fn is_paused(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|session| session.paused)
            .unwrap_or(false)
    }

    /// Forget everything about a session (conversation closed).
    pub fn forget(&self, session_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.remove(session_id);
    }

    pub async fn status(&self, session_id: &str) -> Value {
        let (stopped, paused, snapshot, previous_url) = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match sessions.get(session_id) {
                Some(session) => (
                    session.stopped,
                    session.paused,
                    session.snapshots.back().cloned(),
                    session.last_url.clone(),
                ),
                None => (false, false, None, String::new()),
            }
        };
        let info = self.backend.info();
        let mut value = json!({
            "mode": match self.mode() {
                BrowserUseMode::Off => "off",
                BrowserUseMode::On => "on",
            },
            "tab": self.session_tab(session_id),
            "stopped": stopped,
            "paused": paused,
            "backend": info,
            "snapshot": snapshot.map(|snapshot| json!({
                "id": snapshot.id,
                "url": snapshot.url,
                "title": snapshot.title,
                "nodes": snapshot.nodes.len(),
                "truncated": snapshot.truncated,
            })),
            "init_scripts": self.init_scripts(session_id).len(),
            "capabilities": {
                "snapshot": true,
                "accessibility_tree": true,
                "console": true,
                "network": true,
                "network_bodies": true,
                "request_wait": true,
                "screenshot": true,
                "evaluate": true,
                "evaluate_timeout": true,
                "tabs": true,
                "upload": true,
                "editor": true,
                "drag": true,
                "init_scripts": true,
            },
        });
        if !previous_url.is_empty() {
            value["last_url"] = json!(previous_url);
        }
        value
    }

    /// Gate + pause check shared by every action.
    async fn gate(&self, session_id: &str, cancel: &CancellationToken) -> Result<(), BrowserError> {
        if !self.mode().is_enabled() {
            return Err(BrowserError::Disabled);
        }
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionRuntime::new);
            if session.stopped {
                return Err(BrowserError::Stopped);
            }
        }

        // Wait out a user takeover; re-check stop after every wake-up.
        loop {
            let (paused, notify, stopped) = {
                let sessions = self
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match sessions.get(session_id) {
                    Some(session) => (session.paused, session.notify.clone(), session.stopped),
                    None => return Err(BrowserError::Stopped),
                }
            };
            if stopped {
                return Err(BrowserError::Stopped);
            }
            if !paused {
                break;
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = cancel.cancelled() => return Err(BrowserError::Cancelled),
            }
        }
        self.bind_tab(session_id, cancel).await?;
        self.sync_init_scripts(session_id, cancel).await;
        Ok(())
    }

    /// Scripts this conversation injects into every new document.
    pub fn init_scripts(&self, session_id: &str) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|session| session.init_scripts.clone())
            .unwrap_or_default()
    }

    /// Replace the conversation's init scripts and push them to the backend.
    pub async fn set_init_scripts(
        &self,
        session_id: &str,
        scripts: &[String],
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        self.gate(session_id, cancel).await?;
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = sessions.get_mut(session_id) {
                session.init_scripts = scripts.to_vec();
            }
        }
        with_cancel(cancel, self.backend.set_init_scripts(scripts)).await
    }

    /// Re-assert the conversation's helpers on the tab it is attached to.
    ///
    /// Best effort: a page that cannot take a script must not fail the action
    /// the model actually asked for.
    async fn sync_init_scripts(&self, session_id: &str, cancel: &CancellationToken) {
        let scripts = self.init_scripts(session_id);
        let _ = with_cancel(cancel, self.backend.set_init_scripts(&scripts)).await;
    }

    fn any_session_bound(&self) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|session| session.tab_id.is_some())
    }

    /// Tab the backend is attached to, as far as this runtime knows.
    pub fn active_tab(&self) -> Option<String> {
        self.active_tab
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn remember_active_tab(&self, tab_id: impl Into<String>) {
        *self
            .active_tab
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tab_id.into());
    }

    fn forget_active_tab(&self) {
        *self
            .active_tab
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// Tab bound to one conversation, if any.
    pub fn session_tab(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .and_then(|session| session.tab_id.clone())
    }

    fn set_tab(&self, session_id: &str, tab_id: Option<String>) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.tab_id = tab_id;
        }
    }

    /// Drop per-page state that belongs to the tab we are leaving.
    fn clear_page_state(&self, session_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.snapshots.clear();
            session.frames.clear();
        }
    }

    /// Bind the conversation to its own tab and make it the attached page.
    ///
    /// The first call from a conversation opens a dedicated tab; later calls
    /// switch the backend back to it when another agent acted in between, so
    /// snapshots and `[ref]` handles never leak across conversations.
    async fn bind_tab(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        if let Some(tab_id) = self.session_tab(session_id) {
            if self.active_tab() == Some(tab_id.clone()) {
                return Ok(());
            }
            // The tab may have been closed by the user; fall through and open a
            // fresh one instead of failing every later action.
            if let Ok(info) = with_cancel(cancel, self.backend.select_tab(&tab_id)).await {
                self.note_page(session_id, &info);
                return Ok(());
            }
            self.set_tab(session_id, None);
        }
        // The first conversation takes over the page the browser opened with,
        // so a single user never sees a stray idle tab. Every later conversation
        // gets its own.
        if !self.any_session_bound()
            && let Ok(info) = with_cancel(cancel, self.backend.ensure_ready()).await
            && !info.tab_id.is_empty()
        {
            self.set_tab(session_id, Some(info.tab_id.clone()));
            self.note_page(session_id, &info);
            return Ok(());
        }
        with_cancel(cancel, self.backend.ensure_ready()).await?;
        let tab = with_cancel(cancel, self.backend.new_tab(None)).await?;
        let info = with_cancel(cancel, self.backend.select_tab(&tab.id)).await?;
        self.remember_active_tab(tab.id.clone());
        self.clear_page_state(session_id);
        self.set_tab(session_id, Some(tab.id));
        self.note_page(session_id, &info);
        Ok(())
    }

    fn emit(&self, session_id: &str, kind: OverlayEventKind) {
        let emitter = self
            .overlay
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(emitter) = emitter {
            emitter(OverlayEvent {
                session_id: session_id.to_string(),
                kind,
            });
        }
    }

    fn note(&self, session_id: &str, message: impl Into<String>) {
        let message = message.into();
        let already_visible = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match sessions.get_mut(session_id) {
                Some(session) => {
                    session.last_message = message.clone();
                    session.overlay_visible
                }
                None => false,
            }
        };
        let paused = self.is_paused(session_id);
        let kind = if already_visible {
            OverlayEventKind::Update { message, paused }
        } else {
            OverlayEventKind::Show { message, paused }
        };
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = sessions.get_mut(session_id) {
                session.overlay_visible = true;
            }
        }
        self.emit(session_id, kind);
    }

    fn note_page(&self, session_id: &str, info: &PageInfo) {
        if !info.tab_id.is_empty() {
            self.remember_active_tab(info.tab_id.clone());
        }
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.note_page(info);
        }
    }

    /// Bring the browser up (idempotent) and emit the overlay status line.
    pub async fn ensure_ready(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, "正在连接浏览器…");
        let info = match with_cancel(cancel, self.backend.ensure_ready()).await {
            Ok(info) => info,
            Err(error) => {
                self.emit(session_id, OverlayEventKind::Hide);
                return Err(error);
            }
        };
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        Ok(info)
    }

    pub async fn navigate(
        &self,
        session_id: &str,
        url: &str,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.check_url(url)?;
        self.note(session_id, format!("正在打开 {}", short_url(url)));
        let info = with_cancel(cancel, self.backend.navigate(url, wait)).await?;
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        Ok(info)
    }

    pub async fn reload(
        &self,
        session_id: &str,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let info = with_cancel(cancel, self.backend.reload(wait)).await?;
        self.note_page(session_id, &info);
        Ok(info)
    }

    pub async fn history(
        &self,
        session_id: &str,
        back: bool,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let info = with_cancel(cancel, self.backend.history(back)).await?;
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        Ok(info)
    }

    pub async fn tabs(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<TabInfo>, BrowserError> {
        self.gate(session_id, cancel).await?;
        with_cancel(cancel, self.backend.tabs()).await
    }

    pub async fn new_tab(
        &self,
        session_id: &str,
        url: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<TabInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        if let Some(url) = url {
            self.check_url(url)?;
        }
        let tab = with_cancel(cancel, self.backend.new_tab(url)).await?;
        // A new tab becomes this conversation's tab: the next action would
        // otherwise switch back to the old one and confuse the model.
        let info = with_cancel(cancel, self.backend.select_tab(&tab.id)).await?;
        self.remember_active_tab(tab.id.clone());
        self.clear_page_state(session_id);
        self.set_tab(session_id, Some(tab.id.clone()));
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        Ok(tab)
    }

    pub async fn select_tab(
        &self,
        session_id: &str,
        tab_id: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let info = with_cancel(cancel, self.backend.select_tab(tab_id)).await?;
        self.remember_active_tab(tab_id.to_string());
        self.clear_page_state(session_id);
        self.set_tab(session_id, Some(tab_id.to_string()));
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        Ok(info)
    }

    pub async fn close_tab(
        &self,
        session_id: &str,
        tab_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        self.gate(session_id, cancel).await?;
        with_cancel(cancel, self.backend.close_tab(tab_id)).await?;
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for session in sessions.values_mut() {
            if session.tab_id.as_deref() == Some(tab_id) {
                session.tab_id = None;
                session.snapshots.clear();
                session.frames.clear();
            }
        }
        drop(sessions);
        if self.active_tab() == Some(tab_id.to_string()) {
            self.forget_active_tab();
        }
        Ok(())
    }

    /// Produce and record a snapshot; returns it with its `[ref]` handles.
    pub async fn snapshot(
        &self,
        session_id: &str,
        mode: SnapshotMode,
        max_nodes: usize,
        cancel: &CancellationToken,
    ) -> Result<Snapshot, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, "正在读取页面结构…");
        let mut snapshot = with_cancel(cancel, self.backend.snapshot(mode, max_nodes)).await?;
        self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if snapshot.id.trim().is_empty() {
            snapshot.id = format!("n{}", self.snapshot_counter.load(Ordering::Relaxed));
        }
        let info = PageInfo {
            tab_id: self.session_tab(session_id).unwrap_or_default(),
            url: snapshot.url.clone(),
            title: snapshot.title.clone(),
            ready_state: "complete".to_string(),
            console_errors: 0,
            pending_requests: 0,
        };
        self.note_page(session_id, &info);
        self.note(session_id, describe_page(&info));
        let id = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionRuntime::new);
            session.record_snapshot(snapshot.clone())
        };
        snapshot.id = id;
        Ok(snapshot)
    }

    /// Validate the snapshot id a ref belongs to, then resolve the ref.
    fn resolve_ref(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
    ) -> Result<(), BrowserError> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let session = sessions
            .get(session_id)
            .ok_or_else(|| BrowserError::Invalid("browser session is gone".to_string()))?;
        let snapshot = session.snapshot(snapshot_id)?;
        if snapshot.node(r#ref).is_none() {
            return Err(BrowserError::NotFound(format!(
                "ref `{}` is not in snapshot `{}`; take a new browser_snapshot",
                r#ref, snapshot.id
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn click_ref(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        button: MouseButton,
        click_count: u32,
        modifiers: &[crate::backend::KeyModifier],
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        let label = self
            .ref_label(session_id, r#ref)
            .unwrap_or_else(|| r#ref.to_string());
        self.note(session_id, format!("正在点击 {label}"));
        let info = with_cancel(
            cancel,
            self.backend
                .click_ref(r#ref, button, click_count, modifiers, wait),
        )
        .await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn click_pixel(
        &self,
        session_id: &str,
        frame_id: Option<&str>,
        image_x: f64,
        image_y: f64,
        button: MouseButton,
        click_count: u32,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let (css_x, css_y) = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| BrowserError::Invalid("browser session is gone".to_string()))?;
            // Resolve to CSS pixels using the same raster the model looked at.
            let (scale_x, scale_y, width, height) = {
                let frame = session.frame(frame_id)?;
                (
                    frame.scale_x(),
                    frame.scale_y(),
                    frame.image_width as f64,
                    frame.image_height as f64,
                )
            };
            if !image_x.is_finite() || !image_y.is_finite() {
                return Err(BrowserError::Invalid(
                    "coordinates must be finite numbers".into(),
                ));
            }
            if image_x < 0.0 || image_y < 0.0 || image_x >= width || image_y >= height {
                return Err(BrowserError::Invalid(
                    "coordinates are outside the screenshot frame".into(),
                ));
            }
            (image_x * scale_x, image_y * scale_y)
        };
        self.note(session_id, format!("正在点击 ({css_x:.0}, {css_y:.0})"));
        let info = with_cancel(
            cancel,
            self.backend
                .click_frame_pixel(css_x, css_y, button, click_count, WaitPolicy::None),
        )
        .await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn hover_ref(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        let info = with_cancel(cancel, self.backend.hover_ref(r#ref)).await?;
        Ok(info)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn type_ref(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        text: &str,
        submit: bool,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        let label = self
            .ref_label(session_id, r#ref)
            .unwrap_or_else(|| r#ref.to_string());
        self.note(session_id, format!("正在填写 {label}"));
        let info = with_cancel(cancel, self.backend.type_ref(r#ref, text, submit, wait)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn type_text(
        &self,
        session_id: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, "正在输入…");
        let info = with_cancel(cancel, self.backend.type_text(text)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn press_key(
        &self,
        session_id: &str,
        chord: &KeyChord,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, format!("正在按键 {}", chord.key));
        let info = with_cancel(cancel, self.backend.press_key(chord)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn scroll(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: Option<&str>,
        dx: i32,
        dy: i32,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        if r#ref.is_some() {
            self.resolve_ref(session_id, snapshot_id, r#ref.unwrap_or_default())?;
        }
        let info = with_cancel(cancel, self.backend.scroll(r#ref, dx, dy)).await?;
        Ok(info)
    }

    pub async fn select_option(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        value: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        let info = with_cancel(cancel, self.backend.select_option(r#ref, value)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn set_checked(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        checked: bool,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        let info = with_cancel(cancel, self.backend.set_checked(r#ref, checked)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    pub async fn upload_file(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        path: &std::path::Path,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        self.check_url(&self.current_url(session_id))?;
        self.note(
            session_id,
            format!(
                "正在上传 {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("文件")
            ),
        );
        let info = with_cancel(cancel, self.backend.upload_file(r#ref, path)).await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    /// High-risk: evaluate JS in the page origin.
    ///
    /// `timeout_ms` is the harness budget, not a page-side kill: when it runs
    /// out the outcome is `timed_out` plus whatever the expression logged,
    /// which is far more useful than dropping the evidence.
    pub async fn evaluate(
        &self,
        session_id: &str,
        expression: &str,
        timeout_ms: u64,
        await_promise: bool,
        cancel: &CancellationToken,
    ) -> Result<EvalOutcome, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, "正在执行页面脚本…");
        let request = crate::types::EvalRequest {
            expression: expression.to_string(),
            timeout_ms,
            await_promise,
        };
        with_cancel(cancel, self.backend.evaluate(&request)).await
    }

    /// Body of a finished request.
    pub async fn response_body(
        &self,
        session_id: &str,
        request_id: &str,
        max_chars: usize,
        cancel: &CancellationToken,
    ) -> Result<ResponseBody, BrowserError> {
        self.gate(session_id, cancel).await?;
        with_cancel(cancel, self.backend.response_body(request_id, max_chars)).await
    }

    /// Wait for a response whose URL matches `pattern`.
    pub async fn wait_for_request(
        &self,
        session_id: &str,
        request: &RequestWait,
        cancel: &CancellationToken,
    ) -> Result<ResponseBody, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(session_id, format!("正在等待请求 {}…", request.pattern));
        let body = with_cancel(cancel, self.backend.wait_for_request(request)).await?;
        self.note(session_id, format!("已拿到 {}", short_url(&body.url)));
        Ok(body)
    }

    /// Text of the page element(s) matching a CSS selector.
    pub async fn element_text(
        &self,
        session_id: &str,
        selector: &str,
        all: bool,
        max_chars: usize,
        cancel: &CancellationToken,
    ) -> Result<ElementText, BrowserError> {
        self.gate(session_id, cancel).await?;
        with_cancel(cancel, self.backend.element_text(selector, all, max_chars)).await
    }

    /// Click the element whose accessible name matches `text`.
    ///
    /// Matching is resolved against the latest snapshot, so the click uses the
    /// same box the model saw. Exact matches win over substring matches, and
    /// interactive roles win over decorative ones; the error lists the
    /// candidates so the model can correct itself in one turn.
    pub async fn click_text(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        text: &str,
        exact: bool,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<(String, PageInfo), BrowserError> {
        self.gate(session_id, cancel).await?;
        let node = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session = sessions
                .get(session_id)
                .ok_or_else(|| BrowserError::Invalid("browser session is gone".to_string()))?;
            let snapshot = session.snapshot(snapshot_id)?;
            match_text_node(snapshot, text, exact)?
        };
        let label = format!("{}「{}」", node.role, node.name);
        self.note(session_id, format!("正在点击 {label}"));
        let info = with_cancel(
            cancel,
            self.backend
                .click_ref(&node.id, MouseButton::Left, 1, &[], wait),
        )
        .await?;
        self.after_action(session_id, &info);
        Ok((node.id, info))
    }

    /// Click the center of the first element matching a CSS selector.
    pub async fn click_selector(
        &self,
        session_id: &str,
        selector: &str,
        button: MouseButton,
        click_count: u32,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let bounds = with_cancel(cancel, self.backend.element_box(selector))
            .await?
            .ok_or_else(|| {
                BrowserError::NotFound(format!(
                    "no element matches `{selector}`; check the selector or take a snapshot"
                ))
            })?;
        if !bounds.is_visible() {
            return Err(BrowserError::NotInteractable(format!(
                "`{selector}` has no visible layout box"
            )));
        }
        let (x, y) = bounds.center();
        self.note(session_id, format!("正在点击 {selector}"));
        let info = with_cancel(
            cancel,
            self.backend
                .click_point(x, y, button, click_count, &[], wait),
        )
        .await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    /// Drag from one element/point to another.
    ///
    /// Refs are re-resolved through the backend so the gesture lands on the
    /// element's *current* box, not the box it had when the snapshot was taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn drag(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        from_ref: Option<&str>,
        to_ref: Option<&str>,
        from_point: Option<(f64, f64)>,
        to_point: Option<(f64, f64)>,
        steps: u32,
        wait: WaitPolicy,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        for r#ref in [from_ref, to_ref].into_iter().flatten() {
            self.resolve_ref(session_id, snapshot_id, r#ref)?;
        }
        let start = match (from_ref, from_point) {
            (Some(r#ref), _) => self.ref_center(session_id, r#ref, cancel).await?,
            (None, Some(point)) => point,
            (None, None) => {
                return Err(BrowserError::Invalid(
                    "drag needs from_ref or x/y for the start point".to_string(),
                ));
            }
        };
        let end = match (to_ref, to_point) {
            (Some(r#ref), _) => self.ref_center(session_id, r#ref, cancel).await?,
            (None, Some(point)) => point,
            (None, None) => {
                return Err(BrowserError::Invalid(
                    "drag needs to_ref or to_x/to_y for the end point".to_string(),
                ));
            }
        };
        self.note(
            session_id,
            format!(
                "正在拖动 ({:.0}, {:.0}) → ({:.0}, {:.0})",
                start.0, start.1, end.0, end.1
            ),
        );
        let info = with_cancel(
            cancel,
            self.backend.drag(start, end, steps.clamp(1, 60), wait),
        )
        .await?;
        self.after_action(session_id, &info);
        Ok(info)
    }

    /// Center of a ref's current box.
    async fn ref_center(
        &self,
        session_id: &str,
        r#ref: &str,
        cancel: &CancellationToken,
    ) -> Result<(f64, f64), BrowserError> {
        let bounds = with_cancel(cancel, self.backend.ref_bounds(r#ref))
            .await?
            .ok_or_else(|| {
                BrowserError::NotInteractable(format!(
                    "ref `{}` has no layout box; take a new browser_snapshot",
                    r#ref
                ))
            })?;
        if !bounds.is_visible() {
            return Err(BrowserError::NotInteractable(format!(
                "ref `{}` has no visible layout box",
                r#ref
            )));
        }
        let (x, y) = bounds.center();
        let _ = session_id;
        Ok((x, y))
    }

    /// Drive an in-page code editor.
    pub async fn editor(
        &self,
        session_id: &str,
        request: &EditorRequest,
        cancel: &CancellationToken,
    ) -> Result<Value, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.note(
            session_id,
            match request.op {
                EditorOp::Save => "正在保存编辑器内容…",
                EditorOp::Write => "正在写入编辑器…",
                EditorOp::Read => "正在读取编辑器…",
                EditorOp::List | EditorOp::Detect => "正在查找页面里的编辑器…",
            },
        );
        with_cancel(cancel, self.backend.editor(request)).await
    }

    pub async fn screenshot(
        &self,
        session_id: &str,
        options: ScreenshotOptions,
        cancel: &CancellationToken,
    ) -> Result<PageCapture, BrowserError> {
        self.gate(session_id, cancel).await?;
        let mut capture = with_cancel(cancel, self.backend.screenshot(options)).await?;
        self.frame_counter.fetch_add(1, Ordering::Relaxed);
        if capture.frame.id.trim().is_empty() {
            capture.frame.id = format!("p{}", self.frame_counter.load(Ordering::Relaxed));
        }
        let id = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let session = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionRuntime::new);
            session.record_frame(capture.frame.clone()).id
        };
        capture.frame.id = id;
        Ok(capture)
    }

    /// Persist a raster next to the session's data. Used when the model has no
    /// vision capability or the image exceeds the attachment limit.
    pub fn save_screenshot(&self, session_id: &str, capture: &PageCapture) -> PathBuf {
        let directory = self.data_dir.join("screens").join(sanitize(session_id));
        let path = directory.join(format!("{}.png", sanitize(&capture.frame.id)));
        if std::fs::create_dir_all(&directory).is_ok() {
            let _ = std::fs::write(&path, &capture.png);
        }
        path
    }

    pub async fn console(
        &self,
        session_id: &str,
        query: ConsoleQuery,
        cancel: &CancellationToken,
    ) -> Result<JournalPage<ConsoleEntry>, BrowserError> {
        self.gate(session_id, cancel).await?;
        let page = with_cancel(cancel, self.backend.console(&query)).await?;
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(session_id) {
            session.console_cursor = page.next;
        }
        Ok(page)
    }

    pub async fn network(
        &self,
        session_id: &str,
        query: ConsoleQuery,
        cancel: &CancellationToken,
    ) -> Result<JournalPage<NetEntry>, BrowserError> {
        self.gate(session_id, cancel).await?;
        with_cancel(cancel, self.backend.network(&query)).await
    }

    pub async fn wait_for(
        &self,
        session_id: &str,
        request: &crate::backend::WaitRequest,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let info = with_cancel(cancel, self.backend.wait_for(request)).await?;
        self.note_page(session_id, &info);
        Ok(info)
    }

    pub async fn page_info(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<PageInfo, BrowserError> {
        self.gate(session_id, cancel).await?;
        let info = with_cancel(cancel, self.backend.page_info()).await?;
        self.note_page(session_id, &info);
        Ok(info)
    }

    pub async fn ref_bounds(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
        r#ref: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<Rect>, BrowserError> {
        self.gate(session_id, cancel).await?;
        self.resolve_ref(session_id, snapshot_id, r#ref)?;
        with_cancel(cancel, self.backend.ref_bounds(r#ref)).await
    }

    /// End the browser session for one conversation.
    ///
    /// Other agents may still be driving their own tabs, so this only closes
    /// this conversation's tab; the last one closes the browser process.
    pub async fn end_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let (tab_id, others) = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let tab_id = sessions
                .get(session_id)
                .and_then(|session| session.tab_id.clone());
            let others: Vec<String> = sessions
                .iter()
                .filter(|(id, session)| id.as_str() != session_id && session.tab_id.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            sessions.remove(session_id);
            (tab_id, others)
        };
        self.emit(session_id, OverlayEventKind::Hide);
        if others.is_empty() {
            self.forget_active_tab();
            return self.backend.end_session().await;
        }
        // Detach from our tab before closing it, then hand the browser back to
        // one of the remaining conversations.
        if let Some(tab_id) = tab_id {
            let witness = others
                .iter()
                .find_map(|other| self.session_tab(other))
                .unwrap_or_default();
            if !witness.is_empty() {
                let _ =
                    with_cancel(&CancellationToken::new(), self.backend.select_tab(&witness)).await;
            }
            let _ = self.backend.close_tab(&tab_id).await;
        }
        Ok(())
    }

    fn after_action(&self, session_id: &str, info: &PageInfo) {
        self.note_page(session_id, info);
        let mut message = describe_page(info);
        if info.console_errors > 0 {
            message.push_str(&format!(
                " · {} 条控制台错误（可用 browser_console 查看）",
                info.console_errors
            ));
        }
        self.note(session_id, message);
    }

    fn current_url(&self, session_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|session| session.last_url.clone())
            .unwrap_or_default()
    }

    fn ref_label(&self, session_id: &str, r#ref: &str) -> Option<String> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let session = sessions.get(session_id)?;
        let snapshot = session.snapshots.back()?;
        let node = snapshot.node(r#ref)?;
        Some(format!("{}「{}」", node.role, truncate(&node.name, 24)))
    }

    /// Refuse schemes and hosts the model must not reach on its own.
    fn check_url(&self, url: &str) -> Result<(), BrowserError> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(BrowserError::Invalid("url is empty".into()));
        }
        let lowered = trimmed.to_ascii_lowercase();
        let scheme = lowered
            .split_once(':')
            .map(|(scheme, _)| scheme.to_string())
            .unwrap_or_default();
        match scheme.as_str() {
            "http" | "https" => {}
            "about" => {
                if !lowered.starts_with("about:blank") {
                    return Err(BrowserError::Denied(format!(
                        "`about:` URLs other than about:blank are not allowed ({trimmed})"
                    )));
                }
            }
            "file" => {
                if !self.allow_local_files {
                    return Err(BrowserError::Denied(
                        "file:// URLs are disabled; set EFFIBUDDY_BROWSER_ALLOW_FILE=1 to allow them"
                            .into(),
                    ));
                }
            }
            "chrome" | "edge" | "devtools" | "chrome-extension" | "view-source" | "javascript"
            | "vbscript" | "data" => {
                return Err(BrowserError::Denied(format!(
                    "`{scheme}:` URLs are not allowed ({trimmed})"
                )));
            }
            other => {
                return Err(BrowserError::Invalid(format!(
                    "unsupported URL scheme `{other}`; use http:// or https://"
                )));
            }
        }

        // Cloud metadata endpoints are the classic SSRF target. Loopback is
        // allowed because driving a local dev server is a normal use case.
        if let Some(host) = host_of(&lowered)
            && is_cloud_metadata_host(&host)
        {
            return Err(BrowserError::Denied(format!(
                "`{host}` is a cloud metadata endpoint and is blocked"
            )));
        }
        Ok(())
    }
}

/// Pick the node a `click --text` should hit.
///
/// Exact matches beat substring matches, and interactive roles beat everything
/// else; among equals, the first match in tree order wins (that is the one a
/// user would see first). Failing with the candidate names in the message lets
/// the model fix its call instead of guessing.
fn match_text_node(snapshot: &Snapshot, text: &str, exact: bool) -> Result<AxNode, BrowserError> {
    const INTERACTIVE: [&str; 10] = [
        "button", "link", "menuitem", "tab", "option", "checkbox", "radio", "switch", "textbox",
        "combobox",
    ];
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return Err(BrowserError::Invalid("click text cannot be empty".into()));
    }
    let rank = |node: &AxNode| -> Option<(u8, u8)> {
        let name = node.name.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        let positional = match node.bounds {
            Some(bounds) if bounds.is_visible() => 0u8,
            _ => 1u8,
        };
        let hit = if name == needle {
            0u8
        } else if !exact && name.contains(&needle) {
            1u8
        } else {
            return None;
        };
        let role = if INTERACTIVE.contains(&node.role.as_str()) {
            0u8
        } else {
            1u8
        };
        Some((hit, role.saturating_add(positional)))
    };
    let best = snapshot
        .nodes
        .iter()
        .filter_map(|node| rank(node).map(|rank| (rank, node)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, node)| node.clone());
    if let Some(node) = best {
        return Ok(node);
    }
    let candidates: Vec<String> = snapshot
        .nodes
        .iter()
        .filter(|node| INTERACTIVE.contains(&node.role.as_str()) && !node.name.trim().is_empty())
        .take(12)
        .map(|node| format!("{}「{}」", node.role, truncate(&node.name, 30)))
        .collect();
    Err(BrowserError::NotFound(format!(
        "no element matching text `{}` in snapshot `{}`; visible controls: {}",
        text,
        snapshot.id,
        if candidates.is_empty() {
            "none — take a new snapshot".to_string()
        } else {
            candidates.join(", ")
        }
    )))
}

fn describe_page(info: &PageInfo) -> String {
    let title = if info.title.is_empty() {
        short_url(&info.url)
    } else {
        truncate(&info.title, 40)
    };
    format!("正在浏览 {} · {}", title, short_url(&info.url))
}

fn short_url(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let without_slash = trimmed.strip_suffix('/').unwrap_or(trimmed);
    truncate(without_slash, 60)
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out: String = value.chars().take(limit).collect();
    out.push('…');
    out
}

fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Strip `user:password@`; only the part after the last `@` is the host.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Bracketed IPv6 literals contain colons, so they must be unwrapped before
    // the port split (`[::1]:8080`).
    if let Some(rest) = host.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[..end].to_string());
    }
    Some(host.split(':').next().unwrap_or(host).to_string())
}

fn is_cloud_metadata_host(host: &str) -> bool {
    matches!(
        host,
        "169.254.169.254" | "metadata.google.internal" | "metadata.goog"
    ) || host.starts_with("100.100.100.200")
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Race a backend future against cancellation so a stopped session aborts fast.
async fn with_cancel<F, T>(cancel: &CancellationToken, future: F) -> Result<T, BrowserError>
where
    F: std::future::Future<Output = Result<T, BrowserError>>,
{
    tokio::select! {
        result = future => result,
        _ = cancel.cancelled() => Err(BrowserError::Cancelled),
    }
}

/// Default idle timeout for a single action; the harness has its own tool
/// timeout, so this mainly protects against a hung page.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BrowserError;

    #[test]
    fn url_policy_allows_http_and_blocks_dangerous_schemes() {
        let runtime = BrowserRuntime::with_data_dir(
            Arc::new(crate::backend::UnsupportedBackend::new("test")),
            std::env::temp_dir(),
        );
        assert!(runtime.check_url("https://example.com/a?b=1").is_ok());
        assert!(runtime.check_url("http://localhost:3000").is_ok());
        assert!(runtime.check_url("about:blank").is_ok());
        assert!(matches!(
            runtime.check_url("file:///etc/passwd"),
            Err(BrowserError::Denied(_))
        ));
        assert!(matches!(
            runtime.check_url("chrome://settings"),
            Err(BrowserError::Denied(_))
        ));
        assert!(matches!(
            runtime.check_url("javascript:alert(1)"),
            Err(BrowserError::Denied(_))
        ));
        assert!(matches!(
            runtime.check_url("http://169.254.169.254/latest/meta-data"),
            Err(BrowserError::Denied(_))
        ));
        assert!(matches!(
            runtime.check_url("ftp://example.com"),
            Err(BrowserError::Invalid(_))
        ));
    }

    #[test]
    fn local_files_require_explicit_opt_in() {
        let mut runtime = BrowserRuntime::with_data_dir(
            Arc::new(crate::backend::UnsupportedBackend::new("test")),
            std::env::temp_dir(),
        );
        assert!(runtime.check_url("file:///tmp/a.html").is_err());
        runtime.set_allow_local_files(true);
        assert!(runtime.check_url("file:///tmp/a.html").is_ok());
    }

    #[test]
    fn host_parsing_handles_ports_userinfo_and_ipv6() {
        assert_eq!(
            host_of("https://example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("https://user:pw@example.com/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:8080/").as_deref(), Some("::1"));
        assert_eq!(
            host_of("https://[2606:4700::1111]/x").as_deref(),
            Some("2606:4700::1111")
        );
    }

    #[test]
    fn snapshot_registry_fails_closed_after_eviction() {
        let mut session = SessionRuntime::new();
        for index in 0..(MAX_SNAPSHOTS + 1) {
            session.record_snapshot(Snapshot {
                id: format!("n{index}"),
                url: String::new(),
                title: String::new(),
                mode: SnapshotMode::Interesting,
                nodes: Vec::new(),
                text: None,
                total_nodes: 0,
                truncated: false,
                viewport: None,
            });
        }
        assert!(matches!(
            session.snapshot(Some("n0")),
            Err(BrowserError::StaleSnapshot(_))
        ));
        assert!(session.snapshot(None).is_ok());
    }

    fn fake_runtime() -> (Arc<BrowserRuntime>, Arc<crate::testing::FakeBrowser>) {
        let backend = Arc::new(crate::testing::FakeBrowser::new());
        let runtime = Arc::new(BrowserRuntime::with_data_dir(
            backend.clone(),
            std::env::temp_dir(),
        ));
        (runtime, backend)
    }

    #[tokio::test]
    async fn every_conversation_drives_its_own_tab() {
        let (runtime, backend) = fake_runtime();
        let cancel = CancellationToken::new();

        // Two conversations touching the browser get two tabs.
        runtime.gate("a", &cancel).await.unwrap();
        runtime.gate("b", &cancel).await.unwrap();
        let tab_a = runtime.session_tab("a").expect("a is bound to a tab");
        let tab_b = runtime.session_tab("b").expect("b is bound to a tab");
        assert_ne!(tab_a, tab_b, "conversations must not share a tab");
        assert_eq!(backend.active_tab().as_deref(), Some(tab_b.as_str()));

        // `a` acting again re-attaches to its own tab instead of driving `b`'s.
        runtime.gate("a", &cancel).await.unwrap();
        assert_eq!(backend.active_tab().as_deref(), Some(tab_a.as_str()));
        assert_eq!(runtime.session_tab("a").as_deref(), Some(tab_a.as_str()));
    }

    #[tokio::test]
    async fn a_new_tab_becomes_the_conversations_tab() {
        let (runtime, backend) = fake_runtime();
        let cancel = CancellationToken::new();
        runtime.gate("a", &cancel).await.unwrap();
        let first = runtime.session_tab("a").unwrap();

        let tab = runtime
            .new_tab("a", Some("https://new.test/"), &cancel)
            .await
            .unwrap();
        assert_ne!(tab.id, first);
        assert_eq!(runtime.session_tab("a").as_deref(), Some(tab.id.as_str()));
        assert_eq!(backend.active_tab().as_deref(), Some(tab.id.as_str()));
    }

    #[tokio::test]
    async fn ending_one_conversation_keeps_the_browser_for_the_others() {
        let (runtime, backend) = fake_runtime();
        let cancel = CancellationToken::new();
        runtime.gate("a", &cancel).await.unwrap();
        runtime.gate("b", &cancel).await.unwrap();
        let tab_a = runtime.session_tab("a").unwrap();
        let tab_b = runtime.session_tab("b").unwrap();

        runtime.end_session("a").await.unwrap();
        assert!(runtime.session_tab("a").is_none());
        // `b` still owns its tab and the backend stays attached to it.
        assert_eq!(runtime.session_tab("b").as_deref(), Some(tab_b.as_str()));
        assert_eq!(backend.active_tab().as_deref(), Some(tab_b.as_str()));
        assert!(!backend.tab_ids().contains(&tab_a), "a's tab is closed");
    }

    #[tokio::test]
    async fn init_scripts_follow_the_conversation_and_its_tab() {
        let (runtime, backend) = fake_runtime();
        let cancel = CancellationToken::new();
        runtime
            .set_init_scripts("a", &["globalThis.__set = 1;".to_string()], &cancel)
            .await
            .unwrap();
        assert_eq!(backend.init_scripts(), vec!["globalThis.__set = 1;"]);

        // Another conversation acting must not inherit `a`'s helpers.
        runtime.gate("b", &cancel).await.unwrap();
        assert!(backend.init_scripts().is_empty());

        // Coming back to `a` re-installs them.
        runtime.gate("a", &cancel).await.unwrap();
        assert_eq!(backend.init_scripts(), vec!["globalThis.__set = 1;"]);
    }

    #[tokio::test]
    async fn click_text_prefers_exact_labels_and_reports_candidates() {
        let (runtime, _backend) = fake_runtime();
        let cancel = CancellationToken::new();
        runtime
            .snapshot("a", SnapshotMode::Interesting, 50, &cancel)
            .await
            .unwrap();

        let (r#ref, _info) = runtime
            .click_text("a", None, "save", false, WaitPolicy::None, &cancel)
            .await
            .unwrap();
        assert_eq!(r#ref, "e1");

        let error = runtime
            .click_text("a", None, "Publish", false, WaitPolicy::None, &cancel)
            .await
            .expect_err("unknown label must fail");
        assert!(matches!(error, BrowserError::NotFound(_)), "{error}");
        assert!(error.to_string().contains("button"), "{error}");
    }

    #[tokio::test]
    async fn drag_requires_two_ends_and_resolves_refs() {
        let (runtime, _backend) = fake_runtime();
        let cancel = CancellationToken::new();
        runtime
            .snapshot("a", SnapshotMode::Interesting, 50, &cancel)
            .await
            .unwrap();

        // ref → point works.
        runtime
            .drag(
                "a",
                None,
                Some("e1"),
                None,
                None,
                Some((300.0, 300.0)),
                4,
                WaitPolicy::None,
                &cancel,
            )
            .await
            .unwrap();

        // A missing end fails with an actionable message.
        let error = runtime
            .drag(
                "a",
                None,
                Some("e1"),
                None,
                None,
                None,
                4,
                WaitPolicy::None,
                &cancel,
            )
            .await
            .expect_err("missing end must fail");
        assert!(matches!(error, BrowserError::Invalid(_)), "{error}");

        // A ref from a stale snapshot fails closed.
        let error = runtime
            .drag(
                "a",
                None,
                Some("e99"),
                None,
                None,
                Some((1.0, 1.0)),
                4,
                WaitPolicy::None,
                &cancel,
            )
            .await
            .expect_err("stale ref must fail");
        assert!(matches!(error, BrowserError::NotFound(_)), "{error}");
    }

    #[test]
    fn frame_registry_requires_a_screenshot_first() {
        let session = SessionRuntime::new();
        assert!(matches!(
            session.frame(None),
            Err(BrowserError::StaleFrame(_))
        ));
    }
}
