//! Wayland backend built on XDG Desktop Portals.
//!
//! Wayland does not let a regular client enumerate windows or inject input.
//! The portable path is:
//! - `RemoteDesktop` + `ScreenCast` for keyboard/pointer injection and an
//!   absolute pointer coordinate space (the ScreenCast stream id),
//! - `Screenshot` for pixels.
//!
//! This is intentionally full-screen only. Window-level capture and workspace
//! isolation need compositor-specific protocols and are reported as
//! unsupported instead of silently doing something surprising.

use std::sync::Arc;

use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::{Session, screencast::Stream};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::backend::ComputerBackend;
use crate::encode::decode_crop_encode_png;
use crate::keymap::{KeyChord, Modifier, keysym_for_key, modifier_keysym};
use crate::types::{
    Bounds, Capture, ComputerError, DesktopInfo, DisplayInfo, FrameInfo, FrameTarget,
    InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

const STREAM_DISPLAY_ID: &str = "wayland-stream";

struct PortalState {
    remote: RemoteDesktop<'static>,
    _screencast: Screencast<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    stream_id: u32,
    stream_position: Option<(i32, i32)>,
    stream_size: Option<(i32, i32)>,
}

pub struct WaylandBackend {
    portal: OnceCell<PortalState>,
    /// Native size of the last screenshot. The Screenshot portal may return a
    /// physical-pixel image (e.g. 2560×1600) while RemoteDesktop pointer
    /// coordinates use compositor logical units (e.g. 1920×1200); input is
    /// rescaled from the screenshot space to the stream space.
    last_capture: std::sync::Mutex<Option<(u32, u32)>>,
}

impl WaylandBackend {
    pub fn new() -> Self {
        Self {
            portal: OnceCell::new(),
            last_capture: std::sync::Mutex::new(None),
        }
    }

    async fn portal(&self) -> Result<&PortalState, ComputerError> {
        self.portal.get_or_try_init(Self::open_portal).await
    }

    async fn open_portal() -> Result<PortalState, ComputerError> {
        let remote = RemoteDesktop::new().await.map_err(portal_error)?;
        let screencast = Screencast::new().await.map_err(portal_error)?;
        let session = remote.create_session().await.map_err(portal_error)?;

        remote
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(portal_error)?;
        screencast
            .select_sources(
                &session,
                CursorMode::Hidden,
                SourceType::Monitor.into(),
                true,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(portal_error)?;

        let response = remote
            .start(&session, None)
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        if !response.devices().contains(DeviceType::Keyboard)
            || !response.devices().contains(DeviceType::Pointer)
        {
            return Err(ComputerError::Backend(
                "the desktop portal did not grant keyboard and pointer access".to_string(),
            ));
        }
        let stream = response
            .streams()
            .and_then(|streams| streams.first())
            .ok_or_else(|| {
                ComputerError::Backend(
                    "the screen-cast portal returned no monitor stream".to_string(),
                )
            })?;

        Ok(PortalState {
            remote,
            _screencast: screencast,
            session,
            stream_id: stream.pipe_wire_node_id(),
            stream_position: stream.position(),
            stream_size: stream.size(),
        })
    }

    async fn screenshot_bytes(&self) -> Result<Vec<u8>, ComputerError> {
        let request = Screenshot::request()
            .interactive(false)
            .modal(false)
            .send()
            .await
            .map_err(portal_error)?;
        let response = request.response().map_err(portal_error)?;
        let path = response.uri().to_file_path().map_err(|_| {
            ComputerError::Backend("screenshot portal returned a non-file URI".into())
        })?;
        tokio::fs::read(&path).await.map_err(|error| {
            ComputerError::Backend(format!("cannot read portal screenshot: {error}"))
        })
    }

    fn crop_for_stream(
        state: &PortalState,
        image_width: u32,
        image_height: u32,
    ) -> Option<(i32, i32, u32, u32)> {
        let (x, y) = state.stream_position.unwrap_or((0, 0));
        let (width, height) = state.stream_size?;
        if width <= 0 || height <= 0 {
            return None;
        }
        // The stream reports compositor logical coordinates; the PNG is in
        // physical pixels. Scale the crop if HiDPI made them differ.
        let scale_x = image_width as f64 / width as f64;
        let scale_y = image_height as f64 / height as f64;
        let x = (x as f64 * scale_x).round() as i32;
        let y = (y as f64 * scale_y).round() as i32;
        let width = (width as f64 * scale_x).round() as u32;
        let height = (height as f64 * scale_y).round() as u32;
        Some((x, y, width, height))
    }

    async fn pointer_to(&self, state: &PortalState, x: f64, y: f64) -> Result<(), ComputerError> {
        let (x, y) = match state.stream_size {
            Some((stream_width, stream_height)) if stream_width > 0 && stream_height > 0 => {
                let capture = self
                    .last_capture
                    .lock()
                    .unwrap()
                    .unwrap_or((stream_width as u32, stream_height as u32));
                let scale_x = stream_width as f64 / capture.0.max(1) as f64;
                let scale_y = stream_height as f64 / capture.1.max(1) as f64;
                (x * scale_x, y * scale_y)
            }
            _ => (x, y),
        };
        state
            .remote
            .notify_pointer_motion_absolute(&state.session, state.stream_id, x, y)
            .await
            .map_err(portal_error)
    }

    async fn key(
        &self,
        state: &PortalState,
        keysym: u32,
        state_: KeyState,
    ) -> Result<(), ComputerError> {
        state
            .remote
            .notify_keyboard_keysym(&state.session, keysym as i32, state_)
            .await
            .map_err(portal_error)
    }
}

fn portal_error(error: impl std::fmt::Display) -> ComputerError {
    ComputerError::Backend(format!("desktop portal: {error}"))
}

#[async_trait]
impl ComputerBackend for WaylandBackend {
    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            platform: "linux".to_string(),
            display_server: "wayland".to_string(),
            backend: "xdg-portal".to_string(),
            screen_capture: true,
            window_capture: false,
            window_listing: false,
            input: true,
            desktop_isolation: false,
            isolation_kind: None,
            notes: vec![
                "Wayland 只支持全屏截图与全屏坐标输入；窗口枚举/窗口级截图不可用。".to_string(),
                "首次使用会弹出系统“屏幕共享/远程控制”授权对话框。".to_string(),
                "输入会移动真实的系统指针，请用悬浮条随时接管或停止。".to_string(),
            ],
        }
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        let state = self.portal().await?;
        let (width, height) = state.stream_size.unwrap_or((0, 0));
        if width <= 0 || height <= 0 {
            return Err(ComputerError::Backend(
                "the portal did not report the selected monitor size".to_string(),
            ));
        }
        Ok(vec![DisplayInfo {
            id: STREAM_DISPLAY_ID.to_string(),
            name: "Wayland 屏幕".to_string(),
            bounds: Bounds::new(0, 0, width as u32, height as u32),
            scale: 1.0,
            primary: true,
        }])
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland does not expose a window list to applications; use a full-screen screenshot"
                .to_string(),
        ))
    }

    async fn capture_screen(
        &self,
        display: Option<&str>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        if let Some(display) = display
            && display != STREAM_DISPLAY_ID
        {
            return Err(ComputerError::NotFound(format!("display `{display}`")));
        }
        // Pixel capture must not depend on remote-control permission: the
        // Screenshot portal can succeed even when the user declined keyboard /
        // pointer access. Only consult an *already initialized* RemoteDesktop
        // session for monitor geometry; never block a screenshot on its
        // permission dialog.
        let state = self.portal.get();
        let bytes = self.screenshot_bytes().await?;
        let dimensions = crate::encode::png_dimensions(&bytes)
            .ok_or_else(|| ComputerError::Backend("portal screenshot is not a valid PNG".into()))?;
        let crop = state.and_then(|state| Self::crop_for_stream(state, dimensions.0, dimensions.1));
        let (png, width, height) = decode_crop_encode_png(&bytes, crop, max_dim)?;
        let (native_width, native_height) = state
            .and_then(|state| state.stream_size)
            .map(|(width, height)| (width as u32, height as u32))
            .unwrap_or(dimensions);
        *self.last_capture.lock().unwrap() = Some((native_width, native_height));
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target: FrameTarget::Screen {
                    display_id: Some(STREAM_DISPLAY_ID.to_string()),
                },
                image_width: width,
                image_height: height,
                native_width,
                native_height,
                origin_x: 0,
                origin_y: 0,
                desktop_id: Some("current".to_string()),
            },
        })
    }

    async fn capture_window(
        &self,
        _window_id: &str,
        _max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland cannot capture an arbitrary window without a compositor-specific portal"
                .to_string(),
        ))
    }

    async fn pointer_move(&self, target: &PointerTarget) -> Result<(), ComputerError> {
        let state = self.portal().await?;
        self.pointer_to(state, target.client_x, target.client_y)
            .await
    }

    async fn pointer_button(
        &self,
        target: &PointerTarget,
        button: MouseButton,
        pressed: bool,
        _strategy: InputStrategy,
        modifiers: &[Modifier],
    ) -> Result<(), ComputerError> {
        let state = self.portal().await?;
        if pressed {
            self.pointer_to(state, target.client_x, target.client_y)
                .await?;
            for modifier in modifiers {
                self.key(state, modifier_keysym(*modifier), KeyState::Pressed)
                    .await?;
            }
        }
        state
            .remote
            .notify_pointer_button(
                &state.session,
                button.evdev_button(),
                if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
            )
            .await
            .map_err(portal_error)?;
        if !pressed {
            for modifier in modifiers.iter().rev() {
                self.key(state, modifier_keysym(*modifier), KeyState::Released)
                    .await?;
            }
        }
        Ok(())
    }

    async fn scroll(
        &self,
        target: &PointerTarget,
        dx: i32,
        dy: i32,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        let state = self.portal().await?;
        self.pointer_to(state, target.client_x, target.client_y)
            .await?;
        if dy != 0 {
            state
                .remote
                .notify_pointer_axis_discrete(&state.session, Axis::Vertical, dy.clamp(-50, 50))
                .await
                .map_err(portal_error)?;
        }
        if dx != 0 {
            state
                .remote
                .notify_pointer_axis_discrete(&state.session, Axis::Horizontal, dx.clamp(-50, 50))
                .await
                .map_err(portal_error)?;
        }
        Ok(())
    }

    async fn type_text(
        &self,
        text: &str,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        let state = self.portal().await?;
        for ch in text.chars() {
            let keysym = crate::keymap::char_keysym(ch);
            self.key(state, keysym, KeyState::Pressed).await?;
            tokio::time::sleep(std::time::Duration::from_millis(6)).await;
            self.key(state, keysym, KeyState::Released).await?;
            tokio::time::sleep(std::time::Duration::from_millis(6)).await;
        }
        Ok(())
    }

    async fn press_key(
        &self,
        chord: &KeyChord,
        repeat: u32,
        hold_ms: u64,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        let state = self.portal().await?;
        let keysym = keysym_for_key(&chord.key)
            .ok_or_else(|| ComputerError::Invalid(format!("unknown key `{}`", chord.key)))?;
        for _ in 0..repeat.max(1) {
            for modifier in &chord.modifiers {
                self.key(state, modifier_keysym(*modifier), KeyState::Pressed)
                    .await?;
            }
            self.key(state, keysym, KeyState::Pressed).await?;
            if hold_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
            }
            self.key(state, keysym, KeyState::Released).await?;
            for modifier in chord.modifiers.iter().rev() {
                self.key(state, modifier_keysym(*modifier), KeyState::Released)
                    .await?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(12)).await;
        }
        Ok(())
    }

    async fn activate_window(&self, _window_id: &str) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland does not allow applications to activate arbitrary windows".to_string(),
        ))
    }

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError> {
        Ok(vec![DesktopInfo {
            id: "current".to_string(),
            name: "当前桌面".to_string(),
            index: Some(0),
            active: true,
            supported: false,
            kind: "current".to_string(),
        }])
    }

    async fn lock_isolated_desktop(&self, _name: &str) -> Result<DesktopInfo, ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland cannot move windows to an isolated workspace without compositor support"
                .to_string(),
        ))
    }

    async fn move_window_to_desktop(
        &self,
        _window_id: &str,
        _desktop_id: &str,
    ) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland cannot move arbitrary windows".to_string(),
        ))
    }

    async fn switch_desktop(&self, _desktop_id: &str) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "Wayland cannot switch workspaces programmatically".to_string(),
        ))
    }

    async fn restore_window(&self, _window_id: &str) -> Result<(), ComputerError> {
        Ok(())
    }

    async fn end_isolation(&self) -> Result<(), ComputerError> {
        Ok(())
    }

    async fn pin_overlay(&self, _title: &str) -> Result<(), ComputerError> {
        Ok(())
    }

    fn supports_isolation(&self) -> bool {
        false
    }
}

/// Unused on Wayland but keeps the `Stream` import honest for documentation
/// builds that compile this module with older feature sets.
#[allow(dead_code)]
fn stream_label(stream: &Stream) -> String {
    stream
        .source_type()
        .map(|source| format!("{source:?}"))
        .unwrap_or_else(|| "stream".to_string())
}

#[allow(dead_code)]
fn arc_backend(backend: WaylandBackend) -> Arc<dyn ComputerBackend> {
    Arc::new(backend)
}
