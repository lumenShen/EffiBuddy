//! Deterministic backend used by unit tests and the UI preview mode.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::backend::ComputerBackend;
use crate::encode::encode_rgba_png;
use crate::keymap::{KeyChord, Modifier};
use crate::types::{
    Bounds, Capture, ComputerError, DesktopInfo, DisplayInfo, FrameInfo, FrameTarget,
    InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

#[derive(Default)]
struct FakeState {
    windows: Vec<WindowInfo>,
    clicks: Vec<(f64, f64)>,
    last_strategy: Option<InputStrategy>,
    typed: String,
    keys: Vec<String>,
    parked: Vec<String>,
    moves: usize,
    scrolled: (i32, i32),
    isolation_active: bool,
}

pub struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                windows: vec![WindowInfo {
                    id: "0x1".to_string(),
                    title: "Fake Editor".to_string(),
                    app: "fake".to_string(),
                    pid: Some(4242),
                    bounds: Bounds::new(0, 0, 320, 200),
                    client_bounds: Bounds::new(0, 0, 320, 200),
                    active: true,
                    minimized: false,
                    desktop_id: Some("current".to_string()),
                    capturable: true,
                }],
                ..FakeState::default()
            })),
        }
    }

    pub fn click_calls(&self) -> Vec<(f64, f64)> {
        self.state.lock().unwrap().clicks.clone()
    }

    pub fn last_strategy(&self) -> Option<InputStrategy> {
        self.state.lock().unwrap().last_strategy
    }

    pub fn typed(&self) -> String {
        self.state.lock().unwrap().typed.clone()
    }

    pub fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().keys.clone()
    }

    fn window(&self, window_id: &str) -> Result<WindowInfo, ComputerError> {
        self.state
            .lock()
            .unwrap()
            .windows
            .iter()
            .find(|window| window.id == window_id)
            .cloned()
            .ok_or_else(|| ComputerError::NotFound(format!("window `{window_id}`")))
    }
}

fn checkerboard(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0_u8; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            let light = ((x / 16) + (y / 16)) % 2 == 0;
            let value = if light { 235 } else { 40 };
            rgba[index] = value;
            rgba[index + 1] = value;
            rgba[index + 2] = value;
            rgba[index + 3] = 255;
        }
    }
    rgba
}

#[async_trait]
impl ComputerBackend for FakeBackend {
    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            platform: "test".to_string(),
            display_server: "fake".to_string(),
            backend: "fake".to_string(),
            screen_capture: true,
            window_capture: true,
            window_listing: true,
            input: true,
            desktop_isolation: true,
            isolation_kind: Some("parking".to_string()),
            notes: Vec::new(),
        }
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        Ok(vec![DisplayInfo {
            id: "fake-0".to_string(),
            name: "Fake Display".to_string(),
            bounds: Bounds::new(0, 0, 320, 200),
            scale: 1.0,
            primary: true,
        }])
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        Ok(self.state.lock().unwrap().windows.clone())
    }

    async fn capture_screen(
        &self,
        _display: Option<&str>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let (png, width, height) = encode_rgba_png(checkerboard(320, 200), 320, 200, max_dim)?;
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target: FrameTarget::Screen { display_id: None },
                image_width: width,
                image_height: height,
                native_width: 320,
                native_height: 200,
                origin_x: 0,
                origin_y: 0,
                desktop_id: Some("current".to_string()),
            },
        })
    }

    async fn capture_window(
        &self,
        window_id: &str,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let window = self.window(window_id)?;
        let (png, width, height) = encode_rgba_png(checkerboard(320, 200), 320, 200, max_dim)?;
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target: FrameTarget::Window {
                    window_id: window_id.to_string(),
                },
                image_width: width,
                image_height: height,
                native_width: window.client_bounds.width,
                native_height: window.client_bounds.height,
                origin_x: window.client_bounds.x,
                origin_y: window.client_bounds.y,
                desktop_id: window.desktop_id.clone(),
            },
        })
    }

    async fn pointer_move(&self, _target: &PointerTarget) -> Result<(), ComputerError> {
        self.state.lock().unwrap().moves += 1;
        Ok(())
    }

    async fn pointer_button(
        &self,
        target: &PointerTarget,
        _button: MouseButton,
        pressed: bool,
        strategy: InputStrategy,
        _modifiers: &[Modifier],
    ) -> Result<(), ComputerError> {
        if pressed {
            let mut state = self.state.lock().unwrap();
            state.clicks.push((target.client_x, target.client_y));
            state.last_strategy = Some(strategy);
        }
        Ok(())
    }

    async fn scroll(
        &self,
        _target: &PointerTarget,
        dx: i32,
        dy: i32,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.state.lock().unwrap().scrolled = (dx, dy);
        Ok(())
    }

    async fn type_text(
        &self,
        text: &str,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.state.lock().unwrap().typed.push_str(text);
        Ok(())
    }

    async fn press_key(
        &self,
        chord: &KeyChord,
        _repeat: u32,
        _hold_ms: u64,
        _window_id: Option<&str>,
        _strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        self.state.lock().unwrap().keys.push(chord.key.clone());
        Ok(())
    }

    async fn activate_window(&self, window_id: &str) -> Result<(), ComputerError> {
        self.window(window_id)?;
        Ok(())
    }

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError> {
        let active = self.state.lock().unwrap().isolation_active;
        Ok(vec![
            DesktopInfo {
                id: "current".to_string(),
                name: "当前桌面".to_string(),
                index: Some(0),
                active: !active,
                supported: true,
                kind: "current".to_string(),
            },
            DesktopInfo {
                id: "parked".to_string(),
                name: "EffiBuddy".to_string(),
                index: Some(1),
                active: active,
                supported: true,
                kind: "parking".to_string(),
            },
        ])
    }

    async fn lock_isolated_desktop(&self, name: &str) -> Result<DesktopInfo, ComputerError> {
        self.state.lock().unwrap().isolation_active = true;
        Ok(DesktopInfo {
            id: "parked".to_string(),
            name: name.to_string(),
            index: Some(1),
            active: true,
            supported: true,
            kind: "parking".to_string(),
        })
    }

    async fn move_window_to_desktop(
        &self,
        window_id: &str,
        desktop_id: &str,
    ) -> Result<(), ComputerError> {
        let mut state = self.state.lock().unwrap();
        if let Some(window) = state
            .windows
            .iter_mut()
            .find(|window| window.id == window_id)
        {
            window.desktop_id = Some(desktop_id.to_string());
            if desktop_id == "parked" && !state.parked.iter().any(|id| id == window_id) {
                state.parked.push(window_id.to_string());
            }
            Ok(())
        } else {
            Err(ComputerError::NotFound(format!("window `{window_id}`")))
        }
    }

    async fn switch_desktop(&self, desktop_id: &str) -> Result<(), ComputerError> {
        let mut state = self.state.lock().unwrap();
        state.isolation_active = desktop_id == "parked";
        Ok(())
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), ComputerError> {
        let mut state = self.state.lock().unwrap();
        state.parked.retain(|id| id != window_id);
        if let Some(window) = state
            .windows
            .iter_mut()
            .find(|window| window.id == window_id)
        {
            window.desktop_id = Some("current".to_string());
        }
        Ok(())
    }

    async fn end_isolation(&self) -> Result<(), ComputerError> {
        let mut state = self.state.lock().unwrap();
        state.isolation_active = false;
        let parked = std::mem::take(&mut state.parked);
        for window_id in parked {
            if let Some(window) = state
                .windows
                .iter_mut()
                .find(|window| window.id == window_id)
            {
                window.desktop_id = Some("current".to_string());
            }
        }
        Ok(())
    }

    async fn pin_overlay(&self, _title: &str) -> Result<(), ComputerError> {
        Ok(())
    }

    fn supports_isolation(&self) -> bool {
        true
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}
