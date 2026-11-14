//! X11 backend: EWMH window/workspace management, XGetImage capture and
//! XTest input injection. This is the full-capability Linux path.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::xproto::EventMask;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent,
    ConnectionExt as _, ImageFormat, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
    PropMode, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::backend::ComputerBackend;
use crate::encode::encode_rgba_png;
use crate::keymap::{KeyChord, Modifier, keysym_for_key, modifier_keysym, parse_chord};
use crate::types::{
    Bounds, Capture, ComputerError, DesktopInfo, DisplayInfo, FrameInfo, FrameTarget,
    InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

const CURRENT_TIME: u32 = 0;
const ALL_DESKTOPS: u32 = 0xffff_ffff;
const ROOT_DISPLAY_ID: &str = "x11-root";

#[derive(Debug, Clone, Copy)]
struct Atoms {
    net_client_list: Atom,
    net_wm_name: Atom,
    net_wm_pid: Atom,
    net_active_window: Atom,
    net_wm_desktop: Atom,
    net_current_desktop: Atom,
    net_number_of_desktops: Atom,
    net_desktop_names: Atom,
    net_wm_state: Atom,
    net_wm_state_hidden: Atom,
    net_wm_state_sticky: Atom,
    net_wm_state_above: Atom,
    net_wm_window_type: Atom,
    net_wm_window_type_desktop: Atom,
    net_wm_window_type_dock: Atom,
    net_wm_window_type_toolbar: Atom,
    net_wm_window_type_menu: Atom,
    net_wm_window_type_splash: Atom,
    utf8_string: Atom,
    wm_class: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self, ComputerError> {
        fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom, ComputerError> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        }
        Ok(Self {
            net_client_list: intern(conn, b"_NET_CLIENT_LIST")?,
            net_wm_name: intern(conn, b"_NET_WM_NAME")?,
            net_wm_pid: intern(conn, b"_NET_WM_PID")?,
            net_active_window: intern(conn, b"_NET_ACTIVE_WINDOW")?,
            net_wm_desktop: intern(conn, b"_NET_WM_DESKTOP")?,
            net_current_desktop: intern(conn, b"_NET_CURRENT_DESKTOP")?,
            net_number_of_desktops: intern(conn, b"_NET_NUMBER_OF_DESKTOPS")?,
            net_desktop_names: intern(conn, b"_NET_DESKTOP_NAMES")?,
            net_wm_state: intern(conn, b"_NET_WM_STATE")?,
            net_wm_state_hidden: intern(conn, b"_NET_WM_STATE_HIDDEN")?,
            net_wm_state_sticky: intern(conn, b"_NET_WM_STATE_STICKY")?,
            net_wm_state_above: intern(conn, b"_NET_WM_STATE_ABOVE")?,
            net_wm_window_type: intern(conn, b"_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_desktop: intern(conn, b"_NET_WM_WINDOW_TYPE_DESKTOP")?,
            net_wm_window_type_dock: intern(conn, b"_NET_WM_WINDOW_TYPE_DOCK")?,
            net_wm_window_type_toolbar: intern(conn, b"_NET_WM_WINDOW_TYPE_TOOLBAR")?,
            net_wm_window_type_menu: intern(conn, b"_NET_WM_WINDOW_TYPE_MENU")?,
            net_wm_window_type_splash: intern(conn, b"_NET_WM_WINDOW_TYPE_SPLASH")?,
            utf8_string: intern(conn, b"UTF8_STRING")?,
            wm_class: intern(conn, b"WM_CLASS")?,
        })
    }
}

struct ParkedWindow {
    desktop: u32,
}

pub struct X11Backend {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    atoms: Atoms,
    pid: u32,
    previous_desktop: Mutex<Option<u32>>,
    moved_from: Mutex<HashMap<Window, ParkedWindow>>,
}

impl X11Backend {
    pub fn new() -> Result<Self, ComputerError> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = Atoms::new(&conn)?;
        Ok(Self {
            conn,
            screen_num,
            root,
            atoms,
            pid: std::process::id(),
            previous_desktop: Mutex::new(None),
            moved_from: Mutex::new(HashMap::new()),
        })
    }

    fn screen_size(&self) -> (u32, u32) {
        let screen = &self.conn.setup().roots[self.screen_num];
        (
            screen.width_in_pixels as u32,
            screen.height_in_pixels as u32,
        )
    }

    fn get_u32_property(
        &self,
        window: Window,
        atom: Atom,
    ) -> Result<Option<Vec<u32>>, ComputerError> {
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, 4096)?
            .reply()?;
        if reply.format != 32 {
            return Ok(None);
        }
        Ok(Some(
            reply
                .value32()
                .map(|values| values.collect())
                .unwrap_or_default(),
        ))
    }

    fn get_u8_property(
        &self,
        window: Window,
        atom: Atom,
    ) -> Result<Option<Vec<u8>>, ComputerError> {
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, 16384)?
            .reply()?;
        if reply.format != 8 {
            return Ok(None);
        }
        Ok(Some(
            reply
                .value8()
                .map(|values| values.collect())
                .unwrap_or_default(),
        ))
    }

    fn window_title(&self, window: Window) -> String {
        if let Ok(Some(bytes)) = self.get_u8_property(window, self.atoms.net_wm_name) {
            if let Ok(text) = String::from_utf8(bytes) {
                return text;
            }
        }
        if let Ok(Some(bytes)) = self.get_u8_property(window, AtomEnum::WM_NAME.into()) {
            return String::from_utf8_lossy(&bytes).to_string();
        }
        String::new()
    }

    fn window_class(&self, window: Window) -> String {
        if let Ok(Some(bytes)) = self.get_u8_property(window, self.atoms.wm_class) {
            let parts: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
            if let Some(class) = parts.get(1).filter(|part| !part.is_empty()) {
                return String::from_utf8_lossy(class).to_string();
            }
            if let Some(instance) = parts.first().filter(|part| !part.is_empty()) {
                return String::from_utf8_lossy(instance).to_string();
            }
        }
        String::new()
    }

    fn window_pid(&self, window: Window) -> Option<u32> {
        self.get_u32_property(window, self.atoms.net_wm_pid)
            .ok()
            .flatten()
            .and_then(|values| values.first().copied())
    }

    fn active_window(&self) -> Option<Window> {
        self.get_u32_property(self.root, self.atoms.net_active_window)
            .ok()
            .flatten()
            .and_then(|values| values.first().copied())
            .filter(|window| *window != 0)
    }

    fn client_windows(&self) -> Result<Vec<Window>, ComputerError> {
        Ok(self
            .get_u32_property(self.root, self.atoms.net_client_list)?
            .unwrap_or_default())
    }

    fn window_desktop(&self, window: Window) -> Option<u32> {
        self.get_u32_property(window, self.atoms.net_wm_desktop)
            .ok()
            .flatten()
            .and_then(|values| values.first().copied())
            .filter(|desktop| *desktop != ALL_DESKTOPS)
    }

    fn current_desktop(&self) -> Option<u32> {
        self.get_u32_property(self.root, self.atoms.net_current_desktop)
            .ok()
            .flatten()
            .and_then(|values| values.first().copied())
    }

    fn desktop_count(&self) -> Option<u32> {
        self.get_u32_property(self.root, self.atoms.net_number_of_desktops)
            .ok()
            .flatten()
            .and_then(|values| values.first().copied())
    }

    fn desktop_names(&self) -> Vec<String> {
        let bytes = self
            .get_u8_property(self.root, self.atoms.net_desktop_names)
            .ok()
            .flatten()
            .unwrap_or_default();
        if bytes.is_empty() {
            return Vec::new();
        }
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).to_string())
            .collect()
    }

    fn is_hidden(&self, window: Window) -> bool {
        self.get_u32_property(window, self.atoms.net_wm_state)
            .ok()
            .flatten()
            .map(|states| states.contains(&self.atoms.net_wm_state_hidden))
            .unwrap_or(false)
    }

    fn window_type_excluded(&self, window: Window) -> bool {
        let excluded = [
            self.atoms.net_wm_window_type_desktop,
            self.atoms.net_wm_window_type_dock,
            self.atoms.net_wm_window_type_toolbar,
            self.atoms.net_wm_window_type_menu,
            self.atoms.net_wm_window_type_splash,
        ];
        self.get_u32_property(window, self.atoms.net_wm_window_type)
            .ok()
            .flatten()
            .map(|types| types.iter().any(|atom| excluded.contains(atom)))
            .unwrap_or(false)
    }

    fn window_geometry(&self, window: Window) -> Result<WindowInfo, ComputerError> {
        let geometry = self.conn.get_geometry(window)?.reply()?;
        let translated = self
            .conn
            .translate_coordinates(window, self.root, 0, 0)?
            .reply()?;
        let client_bounds = Bounds::new(
            translated.dst_x as i32,
            translated.dst_y as i32,
            geometry.width as u32,
            geometry.height as u32,
        );
        let desktop = self.window_desktop(window);
        let info = WindowInfo {
            id: window.to_string(),
            title: self.window_title(window),
            app: {
                let class = self.window_class(window);
                if class.is_empty() {
                    self.window_pid(window)
                        .and_then(process_name)
                        .unwrap_or_default()
                } else {
                    class
                }
            },
            pid: self.window_pid(window),
            bounds: client_bounds,
            client_bounds,
            active: self.active_window() == Some(window),
            minimized: self.is_hidden(window),
            desktop_id: desktop.map(|desktop| desktop.to_string()),
            capturable: !self.is_hidden(window),
        };
        Ok(info)
    }

    fn send_root_message(
        &self,
        window: Window,
        type_: Atom,
        data: [u32; 5],
    ) -> Result<(), ComputerError> {
        let event = ClientMessageEvent::new(32, window, type_, data);
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn activate_window_blocking(&self, window: Window) -> Result<(), ComputerError> {
        if let Some(desktop) = self.window_desktop(window)
            && self.current_desktop() != Some(desktop)
        {
            self.send_root_message(
                self.root,
                self.atoms.net_current_desktop,
                [desktop, CURRENT_TIME, 0, 0, 0],
            )?;
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        self.send_root_message(
            window,
            self.atoms.net_active_window,
            [2, CURRENT_TIME, 0, 0, 0],
        )?;
        let _ = self
            .conn
            .set_input_focus(
                x11rb::protocol::xproto::InputFocus::POINTER_ROOT,
                window,
                CURRENT_TIME,
            )?
            .check();
        self.conn.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok(())
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<(), ComputerError> {
        self.conn
            .xtest_fake_input(type_, detail, CURRENT_TIME, self.root, x, y, 0)?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn move_pointer(&self, x: f64, y: f64) -> Result<(), ComputerError> {
        let (width, height) = self.screen_size();
        let x = x.round().clamp(0.0, width.saturating_sub(1) as f64) as i16;
        let y = y.round().clamp(0.0, height.saturating_sub(1) as f64) as i16;
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y)
    }

    fn keymap(&self) -> Result<Keymap, ComputerError> {
        let setup = self.conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
        let reply = self.conn.get_keyboard_mapping(first, count)?.reply()?;
        Ok(Keymap {
            first,
            count,
            per_keycode: reply.keysyms_per_keycode.max(1),
            keysyms: reply.keysyms,
        })
    }

    fn press_keysym(
        &self,
        keysym: u32,
        modifiers: &[Modifier],
        hold_ms: u64,
    ) -> Result<(), ComputerError> {
        let keymap = self.keymap()?;
        let (keycode, level) = if let Some(found) = keymap.find(keysym) {
            found
        } else {
            self.remap_and_press(keysym, modifiers, hold_ms)?;
            return Ok(());
        };
        let mut held = Vec::new();
        for modifier in modifiers {
            if let Some((code, _)) = keymap.find(modifier_keysym(*modifier)) {
                self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
                held.push(code);
            }
        }
        if level >= 1 && !modifiers.contains(&Modifier::Shift) {
            if let Some((code, _)) = keymap.find(modifier_keysym(Modifier::Shift)) {
                self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
                held.push(code);
            }
        }
        self.fake(KEY_PRESS_EVENT, keycode, 0, 0)?;
        if hold_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        self.fake(KEY_RELEASE_EVENT, keycode, 0, 0)?;
        for code in held.into_iter().rev() {
            self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
        }
        Ok(())
    }

    /// Fallback for keysyms absent from the current layout: map a spare
    /// keycode to the keysym, press it, then restore the original mapping.
    fn remap_and_press(
        &self,
        keysym: u32,
        modifiers: &[Modifier],
        hold_ms: u64,
    ) -> Result<(), ComputerError> {
        let keymap = self.keymap()?;
        let (spare, original) = keymap.spare_keycode().ok_or_else(|| {
            ComputerError::Backend("no spare keycode available for Unicode input".to_string())
        })?;
        let remapped = vec![keysym; keymap.per_keycode as usize];
        self.conn
            .change_keyboard_mapping(1, spare, keymap.per_keycode, &remapped)?
            .check()?;
        self.conn.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(8));

        let mut held = Vec::new();
        for modifier in modifiers {
            if let Some((code, _)) = keymap.find(modifier_keysym(*modifier)) {
                self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
                held.push(code);
            }
        }
        self.fake(KEY_PRESS_EVENT, spare, 0, 0)?;
        if hold_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        self.fake(KEY_RELEASE_EVENT, spare, 0, 0)?;
        for code in held.into_iter().rev() {
            self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
        }

        self.conn
            .change_keyboard_mapping(1, spare, keymap.per_keycode, &original)?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn type_char(&self, ch: char) -> Result<(), ComputerError> {
        let keysym = crate::keymap::char_keysym(ch);
        let keymap = self.keymap()?;
        if let Some((code, level)) = keymap.find(keysym) {
            let mut held = Vec::new();
            if level >= 1 {
                if let Some((shift, _)) = keymap.find(modifier_keysym(Modifier::Shift)) {
                    self.fake(KEY_PRESS_EVENT, shift, 0, 0)?;
                    held.push(shift);
                }
            }
            self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
            std::thread::sleep(std::time::Duration::from_millis(6));
            self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
            for code in held.into_iter().rev() {
                self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
            }
            Ok(())
        } else {
            self.remap_and_press(keysym, &[], 0)
        }
    }

    fn capture_drawable(
        &self,
        drawable: u32,
        width: u16,
        height: u16,
        target: FrameTarget,
        origin_x: i32,
        origin_y: i32,
        desktop_id: Option<String>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, 0, 0, width, height, !0)?
            .reply()?;
        let rgba = image_to_rgba(&self.conn, &reply, width, height)?;
        let (png, image_width, image_height) =
            encode_rgba_png(rgba, width as u32, height as u32, max_dim)?;
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target,
                image_width,
                image_height,
                native_width: width as u32,
                native_height: height as u32,
                origin_x,
                origin_y,
                desktop_id,
            },
        })
    }
}

struct Keymap {
    first: u8,
    count: u8,
    per_keycode: u8,
    keysyms: Vec<u32>,
}

impl Keymap {
    fn find(&self, keysym: u32) -> Option<(u8, usize)> {
        if keysym == 0 {
            return None;
        }
        for index in 0..self.count as usize {
            for level in 0..self.per_keycode as usize {
                let offset = index * self.per_keycode as usize + level;
                if self.keysyms.get(offset).copied() == Some(keysym) {
                    return Some(((self.first as usize + index) as u8, level));
                }
            }
        }
        None
    }

    fn spare_keycode(&self) -> Option<(u8, Vec<u32>)> {
        for index in 0..self.count as usize {
            let start = index * self.per_keycode as usize;
            let slice = self.keysyms.get(start..start + self.per_keycode as usize)?;
            if slice.iter().all(|keysym| *keysym == 0) {
                return Some(((self.first as usize + index) as u8, slice.to_vec()));
            }
        }
        None
    }
}

fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim().to_string())
}

fn image_to_rgba(
    conn: &RustConnection,
    reply: &x11rb::protocol::xproto::GetImageReply,
    width: u16,
    height: u16,
) -> Result<Vec<u8>, ComputerError> {
    let setup = conn.setup();
    let bits_per_pixel = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .map(|format| format.bits_per_pixel)
        .unwrap_or(32)
        .max(8) as usize;
    let bytes_per_pixel = bits_per_pixel.div_ceil(8);
    let stride = (width as usize * bits_per_pixel + 31) / 32 * 4;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    let lsb_first = setup.image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST;

    for row in 0..height as usize {
        let row_start = row * stride;
        for column in 0..width as usize {
            let index = row_start + column * bytes_per_pixel;
            if index + bytes_per_pixel > reply.data.len() {
                rgba.extend_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            let pixel = &reply.data[index..index + bytes_per_pixel];
            let (red, green, blue) = match bytes_per_pixel {
                4 => {
                    if lsb_first {
                        (pixel[2], pixel[1], pixel[0])
                    } else {
                        (pixel[1], pixel[2], pixel[3])
                    }
                }
                3 => {
                    if lsb_first {
                        (pixel[2], pixel[1], pixel[0])
                    } else {
                        (pixel[0], pixel[1], pixel[2])
                    }
                }
                2 => {
                    let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                    (
                        (((value >> 11) & 0x1f) * 255 / 31) as u8,
                        (((value >> 5) & 0x3f) * 255 / 63) as u8,
                        ((value & 0x1f) * 255 / 31) as u8,
                    )
                }
                _ => (0, 0, 0),
            };
            rgba.extend_from_slice(&[red, green, blue, 255]);
        }
    }
    Ok(rgba)
}

#[async_trait]
impl ComputerBackend for X11Backend {
    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            platform: "linux".to_string(),
            display_server: "x11".to_string(),
            backend: "x11".to_string(),
            screen_capture: true,
            window_capture: true,
            window_listing: true,
            input: true,
            desktop_isolation: true,
            isolation_kind: Some("ewmh-workspace".to_string()),
            notes: vec![
                "X11: 窗口级截图、XTest 键鼠和专用 EWMH 工作区均可用。".to_string(),
                "切换工作区会改变用户当前看到的桌面；悬浮条会提示。".to_string(),
            ],
        }
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        let (width, height) = self.screen_size();
        Ok(vec![DisplayInfo {
            id: ROOT_DISPLAY_ID.to_string(),
            name: "X11 屏幕".to_string(),
            bounds: Bounds::new(0, 0, width, height),
            scale: 1.0,
            primary: true,
        }])
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        let mut windows = Vec::new();
        for window in self.client_windows()? {
            if self.window_type_excluded(window) {
                continue;
            }
            if self.window_pid(window) == Some(self.pid) {
                continue;
            }
            let title = self.window_title(window);
            let class = self.window_class(window);
            if title.is_empty() && class.is_empty() {
                continue;
            }
            match self.window_geometry(window) {
                Ok(info) if info.client_bounds.width > 0 && info.client_bounds.height > 0 => {
                    windows.push(info)
                }
                _ => {}
            }
        }
        Ok(windows)
    }

    async fn capture_screen(
        &self,
        display: Option<&str>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        if let Some(display) = display
            && display != ROOT_DISPLAY_ID
        {
            return Err(ComputerError::NotFound(format!("display `{display}`")));
        }
        let (width, height) = self.screen_size();
        let desktop = self.current_desktop().map(|desktop| desktop.to_string());
        self.capture_drawable(
            self.root,
            width as u16,
            height as u16,
            FrameTarget::Screen {
                display_id: Some(ROOT_DISPLAY_ID.to_string()),
            },
            0,
            0,
            desktop,
            max_dim,
        )
    }

    async fn capture_window(
        &self,
        window_id: &str,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let window: Window = window_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid X11 window id `{window_id}`")))?;
        let info = self.window_geometry(window)?;
        if info.minimized {
            return Err(ComputerError::Backend(
                "window is minimized; call activate_window first".to_string(),
            ));
        }
        let width = info.client_bounds.width as u16;
        let height = info.client_bounds.height as u16;

        // Prefer the redirected pixmap: it contains the window's own pixels
        // even when another window overlaps it.
        let composite = self
            .conn
            .composite_query_version(0, 4)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        if composite.is_some() {
            if let Ok(pixmap) = self.conn.generate_id() {
                if self
                    .conn
                    .composite_name_window_pixmap(window, pixmap)
                    .ok()
                    .and_then(|cookie| cookie.check().ok())
                    .is_some()
                {
                    let captured = self.capture_drawable(
                        pixmap,
                        width,
                        height,
                        FrameTarget::Window {
                            window_id: window_id.to_string(),
                        },
                        info.client_bounds.x,
                        info.client_bounds.y,
                        info.desktop_id.clone(),
                        max_dim,
                    );
                    let _ = self
                        .conn
                        .free_pixmap(pixmap)
                        .ok()
                        .and_then(|cookie| cookie.check().ok());
                    if captured.is_ok() {
                        return captured;
                    }
                }
            }
        }

        self.capture_drawable(
            window,
            width,
            height,
            FrameTarget::Window {
                window_id: window_id.to_string(),
            },
            info.client_bounds.x,
            info.client_bounds.y,
            info.desktop_id,
            max_dim,
        )
    }

    async fn pointer_move(&self, target: &PointerTarget) -> Result<(), ComputerError> {
        if target.background {
            return Err(ComputerError::Unsupported(
                "X11 cannot inject input into a parked window; use the dedicated workspace"
                    .to_string(),
            ));
        }
        self.move_pointer(target.screen_x, target.screen_y)
    }

    async fn pointer_button(
        &self,
        target: &PointerTarget,
        button: MouseButton,
        pressed: bool,
        strategy: InputStrategy,
        modifiers: &[Modifier],
    ) -> Result<(), ComputerError> {
        if strategy == InputStrategy::Background || target.background {
            return Err(ComputerError::Unsupported(
                "X11 uses foreground XTest input".to_string(),
            ));
        }
        let keymap = self.keymap()?;
        if pressed {
            self.move_pointer(target.screen_x, target.screen_y)?;
            for modifier in modifiers {
                if let Some((code, _)) = keymap.find(modifier_keysym(*modifier)) {
                    self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
                }
            }
        }
        self.fake(
            if pressed {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            },
            button.x11_button(),
            0,
            0,
        )?;
        if !pressed {
            for modifier in modifiers.iter().rev() {
                if let Some((code, _)) = keymap.find(modifier_keysym(*modifier)) {
                    self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
                }
            }
        }
        Ok(())
    }

    async fn scroll(
        &self,
        target: &PointerTarget,
        dx: i32,
        dy: i32,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        if strategy == InputStrategy::Background || target.background {
            return Err(ComputerError::Unsupported(
                "X11 uses foreground XTest input".to_string(),
            ));
        }
        self.move_pointer(target.screen_x, target.screen_y)?;
        let vertical = if dy > 0 { 5 } else { 4 };
        let horizontal = if dx > 0 { 7 } else { 6 };
        for _ in 0..dy.unsigned_abs().min(50) {
            self.fake(BUTTON_PRESS_EVENT, vertical, 0, 0)?;
            self.fake(BUTTON_RELEASE_EVENT, vertical, 0, 0)?;
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        for _ in 0..dx.unsigned_abs().min(50) {
            self.fake(BUTTON_PRESS_EVENT, horizontal, 0, 0)?;
            self.fake(BUTTON_RELEASE_EVENT, horizontal, 0, 0)?;
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        Ok(())
    }

    async fn type_text(
        &self,
        text: &str,
        _window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        if strategy == InputStrategy::Background {
            return Err(ComputerError::Unsupported(
                "X11 typing is always foreground".to_string(),
            ));
        }
        for ch in text.chars() {
            self.type_char(ch)?;
            std::thread::sleep(std::time::Duration::from_millis(6));
        }
        Ok(())
    }

    async fn press_key(
        &self,
        chord: &KeyChord,
        repeat: u32,
        hold_ms: u64,
        _window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        if strategy == InputStrategy::Background {
            return Err(ComputerError::Unsupported(
                "X11 key input is always foreground".to_string(),
            ));
        }
        let keysym = keysym_for_key(&chord.key)
            .ok_or_else(|| ComputerError::Invalid(format!("unknown key `{}`", chord.key)))?;
        for _ in 0..repeat.max(1) {
            self.press_keysym(keysym, &chord.modifiers, hold_ms)?;
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        Ok(())
    }

    async fn activate_window(&self, window_id: &str) -> Result<(), ComputerError> {
        let window: Window = window_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid X11 window id `{window_id}`")))?;
        self.activate_window_blocking(window)
    }

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError> {
        let count = self.desktop_count().unwrap_or(0);
        let current = self.current_desktop().unwrap_or(0);
        let names = self.desktop_names();
        Ok((0..count)
            .map(|index| DesktopInfo {
                id: index.to_string(),
                name: names
                    .get(index as usize)
                    .cloned()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("桌面 {}", index + 1)),
                index: Some(index),
                active: index == current,
                supported: true,
                kind: "workspace".to_string(),
            })
            .collect())
    }

    async fn lock_isolated_desktop(&self, name: &str) -> Result<DesktopInfo, ComputerError> {
        let count = self.desktop_count().unwrap_or(0);
        if count == 0 {
            return Err(ComputerError::Unsupported(
                "the window manager does not expose EWMH desktops".to_string(),
            ));
        }
        let current = self.current_desktop();
        *self.previous_desktop.lock().unwrap() = current;

        let names = self.desktop_names();
        if let Some(index) = names
            .iter()
            .position(|candidate| candidate == name || candidate.contains("EffiBuddy"))
        {
            return Ok(DesktopInfo {
                id: index.to_string(),
                name: names[index].clone(),
                index: Some(index as u32),
                active: current == Some(index as u32),
                supported: true,
                kind: "workspace".to_string(),
            });
        }

        let new_count = count + 1;
        self.send_root_message(
            self.root,
            self.atoms.net_number_of_desktops,
            [new_count, 0, 0, 0, 0],
        )?;
        std::thread::sleep(std::time::Duration::from_millis(180));
        let actual = self.desktop_count().unwrap_or(count);
        if actual <= count {
            return Err(ComputerError::Backend(
                "the window manager refused to create a new desktop".to_string(),
            ));
        }

        // Renaming is best effort: several WMs ignore client writes to
        // _NET_DESKTOP_NAMES.
        let mut names = self.desktop_names();
        while names.len() < actual as usize {
            names.push(String::new());
        }
        if let Some(slot) = names.get_mut((actual - 1) as usize) {
            *slot = name.to_string();
        }
        let mut encoded = Vec::new();
        for entry in &names {
            encoded.extend_from_slice(entry.as_bytes());
            encoded.push(0);
        }
        if let Ok(cookie) = self.conn.change_property8(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_desktop_names,
            self.atoms.utf8_string,
            &encoded,
        ) {
            let _ = cookie.check();
        }
        self.conn.flush()?;

        Ok(DesktopInfo {
            id: (actual - 1).to_string(),
            name: name.to_string(),
            index: Some(actual - 1),
            active: false,
            supported: true,
            kind: "workspace".to_string(),
        })
    }

    async fn move_window_to_desktop(
        &self,
        window_id: &str,
        desktop_id: &str,
    ) -> Result<(), ComputerError> {
        let window: Window = window_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid X11 window id `{window_id}`")))?;
        let desktop: u32 = desktop_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid desktop id `{desktop_id}`")))?;
        let previous = self.window_desktop(window).unwrap_or(0);
        self.moved_from
            .lock()
            .unwrap()
            .insert(window, ParkedWindow { desktop: previous });
        self.send_root_message(window, self.atoms.net_wm_desktop, [desktop, 2, 0, 0, 0])?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }

    async fn switch_desktop(&self, desktop_id: &str) -> Result<(), ComputerError> {
        let desktop: u32 = desktop_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid desktop id `{desktop_id}`")))?;
        self.send_root_message(
            self.root,
            self.atoms.net_current_desktop,
            [desktop, CURRENT_TIME, 0, 0, 0],
        )?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        Ok(())
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), ComputerError> {
        let window: Window = window_id
            .parse()
            .map_err(|_| ComputerError::Invalid(format!("invalid X11 window id `{window_id}`")))?;
        let previous = self
            .moved_from
            .lock()
            .unwrap()
            .remove(&window)
            .map(|parked| parked.desktop);
        if let Some(desktop) = previous {
            self.send_root_message(window, self.atoms.net_wm_desktop, [desktop, 2, 0, 0, 0])?;
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        Ok(())
    }

    async fn end_isolation(&self) -> Result<(), ComputerError> {
        let previous = self.previous_desktop.lock().unwrap().take();
        if let Some(desktop) = previous {
            self.send_root_message(
                self.root,
                self.atoms.net_current_desktop,
                [desktop, CURRENT_TIME, 0, 0, 0],
            )?;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(())
    }

    async fn pin_overlay(&self, title: &str) -> Result<(), ComputerError> {
        for window in self.client_windows()? {
            if self.window_pid(window) != Some(self.pid) {
                continue;
            }
            let window_title = self.window_title(window);
            if !window_title.contains(title) {
                continue;
            }
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms.net_wm_desktop,
                AtomEnum::CARDINAL,
                &[ALL_DESKTOPS],
            );
            let _ = self.send_root_message(
                window,
                self.atoms.net_wm_state,
                [
                    1,
                    self.atoms.net_wm_state_sticky,
                    self.atoms.net_wm_state_above,
                    1,
                    0,
                ],
            );
            self.conn.flush()?;
        }
        Ok(())
    }

    fn supports_isolation(&self) -> bool {
        true
    }
}

/// Parse a chord once so callers can reuse the result.
pub fn chord(input: &str) -> Result<KeyChord, ComputerError> {
    parse_chord(input).map_err(ComputerError::Invalid)
}
