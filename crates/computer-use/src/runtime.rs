//! Frame tracking, pause/stop and action orchestration.
//!
//! Backends are stateless with respect to the agent: every decision about who
//! may control the computer, which frame a coordinate belongs to, and whether
//! the user has taken over lives here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::backend::ComputerBackend;
use crate::frame::{FrameRegistry, map_frame};
use crate::keymap::{KeyChord, Modifier};
use crate::types::{
    Bounds, Capture, ComputerError, ComputerUseMode, DesktopInfo, DisplayInfo, FrameInfo,
    FrameTarget, InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

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
    /// Temporarily close the bar so it does not appear in a full-screen capture.
    CaptureHide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    Screen { display: Option<String> },
    Window { window_id: String },
}

#[derive(Debug, Clone)]
pub struct ClickRequest {
    pub frame_id: Option<String>,
    pub x: f64,
    pub y: f64,
    pub button: MouseButton,
    pub click_count: u32,
    pub modifiers: Vec<Modifier>,
    pub window_id: Option<String>,
    pub strategy: InputStrategy,
}

#[derive(Debug, Clone)]
pub struct DragRequest {
    pub frame_id: Option<String>,
    pub from_x: f64,
    pub from_y: f64,
    pub to_x: f64,
    pub to_y: f64,
    pub duration_ms: u64,
    pub button: MouseButton,
    pub modifiers: Vec<Modifier>,
    pub window_id: Option<String>,
    pub strategy: InputStrategy,
}

#[derive(Debug, Clone)]
pub struct ScrollRequest {
    pub frame_id: Option<String>,
    pub x: f64,
    pub y: f64,
    pub dx: i32,
    pub dy: i32,
    pub window_id: Option<String>,
    pub strategy: InputStrategy,
}

#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub frame_id: Option<String>,
    pub x: f64,
    pub y: f64,
    pub window_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TypeRequest {
    pub text: String,
    pub window_id: Option<String>,
    pub strategy: InputStrategy,
}

#[derive(Debug, Clone)]
pub struct KeyRequest {
    pub chord: KeyChord,
    pub repeat: u32,
    pub hold_ms: u64,
    pub window_id: Option<String>,
    pub strategy: InputStrategy,
}

struct SessionRuntime {
    paused: bool,
    stopped: bool,
    notify: Arc<Notify>,
    frames: FrameRegistry,
    active_desktop: Option<DesktopInfo>,
    previous_desktop: Option<String>,
    moved_windows: Vec<String>,
    last_window_id: Option<String>,
    overlay_visible: bool,
    last_message: String,
}

impl SessionRuntime {
    fn new() -> Self {
        Self {
            paused: false,
            stopped: false,
            notify: Arc::new(Notify::new()),
            frames: FrameRegistry::default(),
            active_desktop: None,
            previous_desktop: None,
            moved_windows: Vec::new(),
            last_window_id: None,
            overlay_visible: false,
            last_message: "正在连接桌面…".to_string(),
        }
    }
}

pub struct ComputerRuntime {
    mode: RwLock<ComputerUseMode>,
    backend: Arc<dyn ComputerBackend>,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    overlay: Mutex<Option<OverlayEmitter>>,
    artifact_dir: PathBuf,
    artifact_counter: AtomicU64,
}

impl ComputerRuntime {
    pub fn new(backend: Arc<dyn ComputerBackend>) -> Self {
        Self::with_artifact_dir(backend, std::env::temp_dir().join("effibuddy-computer"))
    }

    pub fn with_artifact_dir(backend: Arc<dyn ComputerBackend>, artifact_dir: PathBuf) -> Self {
        Self {
            mode: RwLock::new(ComputerUseMode::On),
            backend,
            sessions: Mutex::new(HashMap::new()),
            overlay: Mutex::new(None),
            artifact_dir,
            artifact_counter: AtomicU64::new(0),
        }
    }

    pub fn backend(&self) -> &Arc<dyn ComputerBackend> {
        &self.backend
    }

    pub fn platform_info(&self) -> PlatformInfo {
        self.backend.platform_info()
    }

    pub fn mode(&self) -> ComputerUseMode {
        *self
            .mode
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_mode(&self, mode: ComputerUseMode) {
        *self
            .mode
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
        if mode == ComputerUseMode::Off {
            let ids: Vec<String> = {
                let mut sessions = self.lock_sessions();
                for session in sessions.values_mut() {
                    session.stopped = true;
                    session.paused = false;
                    session.overlay_visible = false;
                    session.notify.notify_waiters();
                }
                sessions.keys().cloned().collect()
            };
            for id in ids {
                self.emit(&id, OverlayEventKind::Hide);
            }
        }
    }

    pub fn set_overlay_emitter(&self, emitter: OverlayEmitter) {
        *self
            .overlay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(emitter);
    }

    pub fn is_stopped(&self, session_id: &str) -> bool {
        self.lock_sessions()
            .get(session_id)
            .map(|session| session.stopped)
            .unwrap_or(false)
    }

    /// Drop all transient state for one conversation (screenshots, parked
    /// windows, pause/stop flags). Approval lives in the tool permission
    /// system, so there is no consent state to clear.
    pub fn forget_session(&self, session_id: &str) {
        if let Some(session) = self.lock_sessions().remove(session_id) {
            session.notify.notify_waiters();
        }
        self.emit(session_id, OverlayEventKind::Hide);
    }

    pub fn take_over(&self, session_id: &str) {
        let paused = {
            let mut sessions = self.lock_sessions();
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            session.paused = true;
            session.last_message =
                "你已接管操作。完成后点「我帮你点好了」继续，或点「停止操作」。".to_string();
            session.overlay_visible
        };
        self.emit(
            session_id,
            if paused {
                OverlayEventKind::Update {
                    message: "你已接管操作。完成后点「我帮你点好了」继续。".to_string(),
                    paused: true,
                }
            } else {
                OverlayEventKind::Show {
                    message: "你已接管操作。完成后点「我帮你点好了」继续。".to_string(),
                    paused: true,
                }
            },
        );
    }

    pub fn resume(&self, session_id: &str) {
        let (visible, message) = {
            let mut sessions = self.lock_sessions();
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            session.paused = false;
            session.last_message = "EffiBuddy 正在继续操作…".to_string();
            session.notify.notify_waiters();
            (session.overlay_visible, session.last_message.clone())
        };
        self.emit(
            session_id,
            if visible {
                OverlayEventKind::Update {
                    message,
                    paused: false,
                }
            } else {
                OverlayEventKind::Show {
                    message,
                    paused: false,
                }
            },
        );
    }

    /// User kill switch. Ends this conversation's drive, wakes waiters and
    /// hides the bar. Approval is unaffected: the next run's calls still go
    /// through the permission mode.
    pub fn stop(&self, session_id: &str) {
        {
            let mut sessions = self.lock_sessions();
            if let Some(session) = sessions.get_mut(session_id) {
                session.stopped = true;
                session.paused = false;
                session.overlay_visible = false;
                session.notify.notify_waiters();
            }
        }
        self.emit(session_id, OverlayEventKind::Hide);
    }

    pub async fn finish_session(&self, session_id: &str) {
        let (moved, was_stopped, desktop_locked) = {
            let sessions = self.lock_sessions();
            match sessions.get(session_id) {
                Some(session) => (
                    session.moved_windows.clone(),
                    session.stopped,
                    session.active_desktop.is_some(),
                ),
                None => (Vec::new(), false, false),
            }
        };

        for window_id in &moved {
            let _ = self.backend.restore_window(window_id).await;
        }
        if desktop_locked {
            let _ = self.backend.end_isolation().await;
        }

        {
            let mut sessions = self.lock_sessions();
            if let Some(session) = sessions.get_mut(session_id) {
                session.notify.notify_waiters();
            }
            if was_stopped {
                sessions.remove(session_id);
            } else if let Some(session) = sessions.get_mut(session_id) {
                session.frames.clear();
                session.paused = false;
                session.stopped = false;
                session.active_desktop = None;
                session.previous_desktop = None;
                session.moved_windows.clear();
                session.overlay_visible = false;
            }
        }
        self.emit(session_id, OverlayEventKind::Hide);
    }

    /// Restore parked windows/workspace without changing consent. Used when
    /// switching conversations.
    pub async fn restore_isolation(&self, session_id: &str) {
        let moved = {
            let sessions = self.lock_sessions();
            sessions
                .get(session_id)
                .map(|session| session.moved_windows.clone())
                .unwrap_or_default()
        };
        for window_id in &moved {
            let _ = self.backend.restore_window(window_id).await;
        }
        let _ = self.backend.end_isolation().await;
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.moved_windows.clear();
            session.active_desktop = None;
            session.previous_desktop = None;
            session.overlay_visible = false;
        }
        self.emit(session_id, OverlayEventKind::Hide);
    }

    pub async fn wait_turn(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.ensure_ready(session_id)?;
        loop {
            let (notify, paused, stopped) = {
                let sessions = self.lock_sessions();
                let session = sessions
                    .get(session_id)
                    .ok_or_else(|| ComputerError::Invalid("session runtime is gone".to_string()))?;
                (session.notify.clone(), session.paused, session.stopped)
            };
            if stopped {
                return Err(ComputerError::Stopped);
            }
            if !paused {
                return Ok(());
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = cancel.cancelled() => return Err(ComputerError::Cancelled),
            }
        }
    }

    pub async fn status(&self, session_id: &str) -> Result<Value, ComputerError> {
        self.ensure_ready(session_id)?;
        let mode = self.mode();
        if mode.is_enabled() {
            self.show_overlay(session_id, "正在检查电脑操作能力…");
        }
        let (paused, stopped, desktop, moved) = {
            let sessions = self.lock_sessions();
            match sessions.get(session_id) {
                Some(session) => (
                    session.paused,
                    session.stopped,
                    session.active_desktop.clone(),
                    session.moved_windows.clone(),
                ),
                None => (false, false, None, Vec::new()),
            }
        };
        let mut value = json!({
            "mode": mode,
            "paused": paused,
            "stopped": stopped,
            "active_desktop": desktop,
            "moved_windows": moved,
            "platform": self.backend.platform_info(),
        });
        if let Ok(desktops) = self.backend.list_desktops().await {
            value["desktops"] = serde_json::to_value(desktops).unwrap_or(Value::Null);
        }
        Ok(value)
    }

    pub async fn list_displays(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<DisplayInfo>, ComputerError> {
        self.wait_turn(session_id, cancel).await?;
        self.show_overlay(session_id, "正在读取显示器信息…");
        self.backend.list_displays().await
    }

    pub async fn list_windows(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<WindowInfo>, ComputerError> {
        self.wait_turn(session_id, cancel).await?;
        self.show_overlay(session_id, "正在读取窗口列表…");
        self.backend.list_windows().await
    }

    pub async fn list_desktops(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<DesktopInfo>, ComputerError> {
        self.wait_turn(session_id, cancel).await?;
        self.show_overlay(session_id, "正在读取桌面列表…");
        self.backend.list_desktops().await
    }

    pub async fn screenshot(
        &self,
        session_id: &str,
        target: CaptureTarget,
        max_dim: Option<u32>,
        cancel: &CancellationToken,
    ) -> Result<Capture, ComputerError> {
        self.begin_action(session_id, cancel, "正在读取屏幕…")
            .await?;

        let hide_overlay = matches!(target, CaptureTarget::Screen { .. });
        if hide_overlay {
            self.emit(session_id, OverlayEventKind::CaptureHide);
            tokio::time::sleep(Duration::from_millis(180)).await;
        }

        let result = match &target {
            CaptureTarget::Screen { display } => {
                self.backend
                    .capture_screen(display.as_deref(), max_dim)
                    .await
            }
            CaptureTarget::Window { window_id } => {
                self.backend.capture_window(window_id, max_dim).await
            }
        };

        if hide_overlay {
            self.restore_overlay(session_id);
        }

        let mut capture = result?;
        let active_desktop = {
            let sessions = self.lock_sessions();
            sessions
                .get(session_id)
                .and_then(|session| session.active_desktop.clone())
        };
        if capture.frame.desktop_id.is_none() {
            capture.frame.desktop_id = active_desktop.map(|desktop| desktop.id);
        }
        let frame = {
            let mut sessions = self.lock_sessions();
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| ComputerError::Invalid("session runtime is gone".to_string()))?;
            session.frames.record(capture.frame.clone())
        };
        capture.frame = frame;
        self.update_overlay(session_id, "已获取截图");
        Ok(capture)
    }

    pub async fn click(
        &self,
        session_id: &str,
        request: ClickRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在点击…").await?;
        let (target, _, background) = self
            .resolve_target(
                session_id,
                request.frame_id.as_deref(),
                request.x,
                request.y,
                request.window_id.as_deref(),
            )
            .await?;
        let strategy = resolve_strategy(request.strategy, background)?;
        self.prepare_foreground(&target, strategy).await;

        let count = request.click_count.clamp(1, 3);
        for index in 0..count {
            self.backend
                .pointer_button(&target, request.button, true, strategy, &request.modifiers)
                .await?;
            tokio::time::sleep(Duration::from_millis(35)).await;
            self.backend
                .pointer_button(&target, request.button, false, strategy, &request.modifiers)
                .await?;
            if index + 1 < count {
                tokio::time::sleep(Duration::from_millis(70)).await;
            }
        }
        self.update_overlay(session_id, "点击完成，等待观察结果");
        Ok(())
    }

    pub async fn drag(
        &self,
        session_id: &str,
        request: DragRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在拖动…").await?;
        let (start, frame, background) = self
            .resolve_target(
                session_id,
                request.frame_id.as_deref(),
                request.from_x,
                request.from_y,
                request.window_id.as_deref(),
            )
            .await?;
        let end = map_frame(
            &frame,
            request
                .to_x
                .clamp(0.0, frame.image_width.saturating_sub(1) as f64),
            request
                .to_y
                .clamp(0.0, frame.image_height.saturating_sub(1) as f64),
        )?;
        let end = PointerTarget {
            background: start.background,
            ..end
        };
        let strategy = resolve_strategy(request.strategy, background)?;
        self.prepare_foreground(&start, strategy).await;

        self.backend
            .pointer_button(&start, request.button, true, strategy, &request.modifiers)
            .await?;

        let steps = (request.duration_ms / 25).clamp(8, 80) as u32;
        for index in 1..=steps {
            let fraction = index as f64 / steps as f64;
            let point = PointerTarget {
                screen_x: start.screen_x + (end.screen_x - start.screen_x) * fraction,
                screen_y: start.screen_y + (end.screen_y - start.screen_y) * fraction,
                client_x: start.client_x + (end.client_x - start.client_x) * fraction,
                client_y: start.client_y + (end.client_y - start.client_y) * fraction,
                window_id: start.window_id.clone(),
                background: start.background,
                desktop_id: start.desktop_id.clone(),
            };
            if let Err(error) = self.backend.pointer_move(&point).await {
                let _ = self
                    .backend
                    .pointer_button(&end, request.button, false, strategy, &request.modifiers)
                    .await;
                return Err(error);
            }
            tokio::time::sleep(Duration::from_millis(
                (request.duration_ms / steps as u64).max(8),
            ))
            .await;
        }

        self.backend
            .pointer_button(&end, request.button, false, strategy, &request.modifiers)
            .await?;
        self.update_overlay(session_id, "拖动完成");
        Ok(())
    }

    pub async fn scroll(
        &self,
        session_id: &str,
        request: ScrollRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在滚动…").await?;
        let (target, _, background) = self
            .resolve_target(
                session_id,
                request.frame_id.as_deref(),
                request.x,
                request.y,
                request.window_id.as_deref(),
            )
            .await?;
        let strategy = resolve_strategy(request.strategy, background)?;
        self.prepare_foreground(&target, strategy).await;
        self.backend
            .scroll(&target, request.dx, request.dy, strategy)
            .await?;
        self.update_overlay(session_id, "滚动完成");
        Ok(())
    }

    pub async fn pointer_move(
        &self,
        session_id: &str,
        request: MoveRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在移动指针…")
            .await?;
        let (target, _, _) = self
            .resolve_target(
                session_id,
                request.frame_id.as_deref(),
                request.x,
                request.y,
                request.window_id.as_deref(),
            )
            .await?;
        self.backend.pointer_move(&target).await?;
        Ok(())
    }

    pub async fn type_text(
        &self,
        session_id: &str,
        request: TypeRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在输入文本…")
            .await?;
        let background = self.window_is_background(session_id, request.window_id.as_deref());
        let strategy = resolve_strategy(request.strategy, background)?;
        if let Some(window_id) = request.window_id.as_deref() {
            self.ensure_foreground_window(window_id, strategy).await;
        }
        self.backend
            .type_text(&request.text, request.window_id.as_deref(), strategy)
            .await?;
        self.update_overlay(session_id, "文本输入完成");
        Ok(())
    }

    pub async fn press_key(
        &self,
        session_id: &str,
        request: KeyRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在按键…").await?;
        let background = self.window_is_background(session_id, request.window_id.as_deref());
        let strategy = resolve_strategy(request.strategy, background)?;
        if let Some(window_id) = request.window_id.as_deref() {
            self.ensure_foreground_window(window_id, strategy).await;
        }
        self.backend
            .press_key(
                &request.chord,
                request.repeat.max(1),
                request.hold_ms,
                request.window_id.as_deref(),
                strategy,
            )
            .await?;
        self.update_overlay(session_id, "按键完成");
        Ok(())
    }

    pub async fn activate_window(
        &self,
        session_id: &str,
        window_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在切换窗口…")
            .await?;
        self.backend.activate_window(window_id).await?;
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.last_window_id = Some(window_id.to_string());
            session.moved_windows.retain(|id| id != window_id);
        }
        self.update_overlay(session_id, "窗口已切换");
        Ok(())
    }

    pub async fn lock_desktop(
        &self,
        session_id: &str,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<DesktopInfo, ComputerError> {
        self.begin_action(session_id, cancel, "正在锁定隔离工作区…")
            .await?;
        let desktops = self.backend.list_desktops().await.unwrap_or_default();
        let previous = desktops
            .iter()
            .find(|desktop| desktop.active)
            .map(|desktop| desktop.id.clone());
        let desktop = self.backend.lock_isolated_desktop(name).await?;
        if desktop.kind == "workspace" {
            self.backend.switch_desktop(&desktop.id).await?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Keep the control bar visible on the new workspace when the WM
            // honors EWMH sticky/above hints.
            let _ = self.backend.pin_overlay("EffiBuddy Computer Use").await;
        }
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.previous_desktop = previous;
            session.active_desktop = Some(desktop.clone());
        }
        self.update_overlay(session_id, &format!("已锁定隔离工作区「{}」", desktop.name));
        Ok(desktop)
    }

    pub async fn move_window_to_desktop(
        &self,
        session_id: &str,
        window_id: &str,
        desktop_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在移动窗口到隔离工作区…")
            .await?;
        self.backend
            .move_window_to_desktop(window_id, desktop_id)
            .await?;
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            if !session.moved_windows.iter().any(|id| id == window_id) {
                session.moved_windows.push(window_id.to_string());
            }
            session.last_window_id = Some(window_id.to_string());
        }
        self.update_overlay(session_id, "窗口已移入隔离工作区");
        Ok(())
    }

    pub async fn restore_window(
        &self,
        session_id: &str,
        window_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在恢复窗口…")
            .await?;
        self.backend.restore_window(window_id).await?;
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.moved_windows.retain(|id| id != window_id);
        }
        self.update_overlay(session_id, "窗口已恢复");
        Ok(())
    }

    pub async fn switch_desktop(
        &self,
        session_id: &str,
        desktop_id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ComputerError> {
        self.begin_action(session_id, cancel, "正在切换桌面…")
            .await?;
        self.backend.switch_desktop(desktop_id).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        let _ = self.backend.pin_overlay("EffiBuddy Computer Use").await;
        self.update_overlay(session_id, "桌面已切换");
        Ok(())
    }

    pub async fn end_session(&self, session_id: &str) {
        self.finish_session(session_id).await;
    }

    pub async fn pin_overlay(&self, title: &str) {
        let _ = self.backend.pin_overlay(title).await;
    }

    /// Persist a screenshot for a model without vision support.
    pub fn save_screenshot(&self, session_id: &str, capture: &Capture) -> PathBuf {
        let index = self.artifact_counter.fetch_add(1, Ordering::Relaxed);
        let safe_session: String = session_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let dir = self.artifact_dir.join(safe_session);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}-{index}.png", capture.frame.id));
        let _ = std::fs::write(&path, &capture.png);
        path
    }

    fn ensure_session(&self, session_id: &str) {
        let mut sessions = self.lock_sessions();
        sessions
            .entry(session_id.to_string())
            .or_insert_with(SessionRuntime::new);
    }

    fn ensure_ready(&self, session_id: &str) -> Result<(), ComputerError> {
        self.ensure_session(session_id);
        let mode = self.mode();
        if !mode.is_enabled() {
            return Err(ComputerError::Disabled);
        }
        let sessions = self.lock_sessions();
        let session = sessions
            .get(session_id)
            .ok_or_else(|| ComputerError::Invalid("session runtime is gone".to_string()))?;
        if session.stopped {
            return Err(ComputerError::Stopped);
        }
        Ok(())
    }

    async fn begin_action(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
        message: &str,
    ) -> Result<(), ComputerError> {
        self.wait_turn(session_id, cancel).await?;
        self.show_overlay(session_id, message);
        self.ensure_locked_desktop(session_id).await?;
        Ok(())
    }

    async fn ensure_locked_desktop(&self, session_id: &str) -> Result<(), ComputerError> {
        let desktop = {
            let sessions = self.lock_sessions();
            sessions
                .get(session_id)
                .and_then(|session| session.active_desktop.clone())
        };
        if let Some(desktop) = desktop
            && desktop.kind == "workspace"
        {
            let active = self
                .backend
                .list_desktops()
                .await
                .ok()
                .and_then(|desktops| {
                    desktops
                        .into_iter()
                        .find(|candidate| candidate.id == desktop.id)
                        .map(|candidate| candidate.active)
                })
                .unwrap_or(false);
            if !active {
                self.backend.switch_desktop(&desktop.id).await?;
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        }
        Ok(())
    }

    async fn resolve_target(
        &self,
        session_id: &str,
        frame_id: Option<&str>,
        x: f64,
        y: f64,
        requested_window: Option<&str>,
    ) -> Result<(PointerTarget, FrameInfo, bool), ComputerError> {
        let (frame, background_session) = {
            let sessions = self.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ComputerError::Invalid("session runtime is gone".to_string()))?;
            (
                session.frames.resolve(frame_id)?,
                session
                    .active_desktop
                    .as_ref()
                    .is_some_and(|desktop| desktop.kind == "parking"),
            )
        };

        if let Some(requested) = requested_window {
            if frame.window_id() != Some(requested) {
                return Err(ComputerError::StaleFrame(format!(
                    "frame belongs to a different window than `{requested}`"
                )));
            }
        }

        if let FrameTarget::Window { window_id } = &frame.target {
            let info = self.backend.window_info(window_id).await.map_err(|_| {
                ComputerError::StaleFrame(format!("window `{window_id}` is no longer available"))
            })?;
            let expected = Bounds::new(
                frame.origin_x,
                frame.origin_y,
                frame.native_width,
                frame.native_height,
            );
            if !info.client_bounds.approximately_eq(&expected) {
                return Err(ComputerError::StaleFrame(
                    "the window moved or resized since the screenshot".to_string(),
                ));
            }
        }

        let mut target = map_frame(&frame, x, y)?;
        let parked = frame.desktop_id.as_deref() == Some("parked")
            || (background_session
                && target
                    .window_id
                    .as_deref()
                    .map(|id| {
                        self.lock_sessions()
                            .get(session_id)
                            .map(|session| session.moved_windows.iter().any(|moved| moved == id))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false));
        target.background = parked;
        target.desktop_id = frame.desktop_id.clone();

        if let Some(window_id) = &target.window_id
            && let Some(session) = self.lock_sessions().get_mut(session_id)
        {
            session.last_window_id = Some(window_id.clone());
        }
        Ok((target, frame, parked))
    }

    async fn prepare_foreground(&self, target: &PointerTarget, strategy: InputStrategy) {
        if strategy != InputStrategy::Foreground || target.background {
            return;
        }
        if let Some(window_id) = &target.window_id {
            let _ = self.backend.activate_window(window_id).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn ensure_foreground_window(&self, window_id: &str, strategy: InputStrategy) {
        if strategy == InputStrategy::Foreground {
            let _ = self.backend.activate_window(window_id).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn window_is_background(&self, session_id: &str, window_id: Option<&str>) -> bool {
        let Some(window_id) = window_id else {
            return false;
        };
        let sessions = self.lock_sessions();
        let Some(session) = sessions.get(session_id) else {
            return false;
        };
        let parking = session
            .active_desktop
            .as_ref()
            .is_some_and(|desktop| desktop.kind == "parking");
        parking && session.moved_windows.iter().any(|id| id == window_id)
    }

    fn show_overlay(&self, session_id: &str, message: &str) {
        let was_visible = {
            let mut sessions = self.lock_sessions();
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            let was_visible = session.overlay_visible;
            session.overlay_visible = true;
            session.last_message = message.to_string();
            was_visible
        };
        let paused = self
            .lock_sessions()
            .get(session_id)
            .map(|session| session.paused)
            .unwrap_or(false);
        self.emit(
            session_id,
            if was_visible {
                OverlayEventKind::Update {
                    message: message.to_string(),
                    paused,
                }
            } else {
                OverlayEventKind::Show {
                    message: message.to_string(),
                    paused,
                }
            },
        );
    }

    fn update_overlay(&self, session_id: &str, message: &str) {
        self.show_overlay(session_id, message);
    }

    fn restore_overlay(&self, session_id: &str) {
        let (message, paused) = {
            let mut sessions = self.lock_sessions();
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            session.overlay_visible = true;
            (session.last_message.clone(), session.paused)
        };
        self.emit(session_id, OverlayEventKind::Show { message, paused });
    }

    fn emit(&self, session_id: &str, kind: OverlayEventKind) {
        let emitter = self
            .overlay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(emitter) = emitter {
            emitter(OverlayEvent {
                session_id: session_id.to_string(),
                kind,
            });
        }
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionRuntime>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn resolve_strategy(
    requested: InputStrategy,
    background: bool,
) -> Result<InputStrategy, ComputerError> {
    match requested {
        InputStrategy::Auto => Ok(if background {
            InputStrategy::Background
        } else {
            InputStrategy::Foreground
        }),
        InputStrategy::Background if !background => Err(ComputerError::Invalid(
            "background strategy requires a window parked in the isolated lane".to_string(),
        )),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeBackend;

    fn runtime() -> (Arc<ComputerRuntime>, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new());
        let runtime = Arc::new(ComputerRuntime::new(backend.clone()));
        (runtime, backend)
    }

    #[tokio::test]
    async fn wait_turn_requires_only_an_enabled_capability() {
        let (runtime, _) = runtime();
        let cancel = CancellationToken::new();
        // No per-session consent: an enabled capability is ready on first use;
        // approval belongs to the tool permission mode.
        assert!(runtime.wait_turn("s", &cancel).await.is_ok());
        runtime.set_mode(ComputerUseMode::Off);
        assert_eq!(
            runtime.wait_turn("s", &cancel).await,
            Err(ComputerError::Disabled)
        );
    }

    #[tokio::test]
    async fn status_reports_the_device_without_a_consent_flag() {
        let (runtime, _) = runtime();
        let status = runtime.status("s").await.unwrap();
        assert!(status.get("consent").is_none());
        assert_eq!(status["platform"]["backend"], "fake");
    }

    #[tokio::test]
    async fn takeover_pauses_until_resume_or_stop() {
        let (runtime, _) = runtime();
        // The session exists once the tool has been used at least once.
        runtime.wait_turn("s", &CancellationToken::new()).await.unwrap();
        runtime.take_over("s");
        let waiter = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.wait_turn("s", &CancellationToken::new()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        runtime.resume("s");
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        runtime.take_over("s");
        runtime.stop("s");
        assert_eq!(
            runtime.wait_turn("s", &CancellationToken::new()).await,
            Err(ComputerError::Stopped)
        );
    }

    #[tokio::test]
    async fn screenshot_records_frame_and_click_maps_coordinates() {
        let (runtime, backend) = runtime();
        let capture = runtime
            .screenshot(
                "s",
                CaptureTarget::Screen { display: None },
                Some(1280),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!capture.frame.id.is_empty());
        runtime
            .click(
                "s",
                ClickRequest {
                    frame_id: Some(capture.frame.id.clone()),
                    x: 10.0,
                    y: 20.0,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                    window_id: None,
                    strategy: InputStrategy::Foreground,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let calls = backend.click_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 10.0);
        assert_eq!(calls[0].1, 20.0);
    }

    #[tokio::test]
    async fn stale_frame_is_rejected_after_eviction() {
        let (runtime, _) = runtime();
        let mut first = String::new();
        for _ in 0..(crate::frame::MAX_FRAMES + 1) {
            let capture = runtime
                .screenshot(
                    "s",
                    CaptureTarget::Screen { display: None },
                    None,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            if first.is_empty() {
                first = capture.frame.id.clone();
            }
        }
        let error = runtime
            .click(
                "s",
                ClickRequest {
                    frame_id: Some(first),
                    x: 1.0,
                    y: 1.0,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                    window_id: None,
                    strategy: InputStrategy::Foreground,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ComputerError::StaleFrame(_)));
    }

    #[tokio::test]
    async fn parking_lane_uses_background_strategy() {
        let (runtime, backend) = runtime();
        let desktop = runtime
            .lock_desktop("s", "EffiBuddy", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(desktop.kind, "parking");
        runtime
            .move_window_to_desktop("s", "0x1", "parked", &CancellationToken::new())
            .await
            .unwrap();
        let capture = runtime
            .screenshot(
                "s",
                CaptureTarget::Window {
                    window_id: "0x1".to_string(),
                },
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        runtime
            .click(
                "s",
                ClickRequest {
                    frame_id: Some(capture.frame.id),
                    x: 5.0,
                    y: 5.0,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                    window_id: Some("0x1".to_string()),
                    strategy: InputStrategy::Auto,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(backend.last_strategy(), Some(InputStrategy::Background));
    }

    #[tokio::test]
    async fn off_mode_blocks_everything() {
        let (runtime, _) = runtime();
        runtime.set_mode(ComputerUseMode::Off);
        let error = runtime
            .screenshot(
                "s",
                CaptureTarget::Screen { display: None },
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, ComputerError::Disabled);
    }
}
