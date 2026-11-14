//! Windows backend: Win32 window enumeration, GDI/PrintWindow capture,
//! SendInput foreground input and a background "parking lane" for windows that
//! should not disturb the user.
//!
//! The parking lane is a deliberate choice over undocumented virtual-desktop
//! COM interfaces: it works on every supported Windows version, never switches
//! the user's desktop, and keeps the window off-screen while the agent works.
//! Apps that ignore `WM_*` messages can be driven with
//! `strategy=foreground`, which temporarily restores the window.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use winapi::shared::minwindef::{BOOL, DWORD, FALSE, LPARAM, TRUE, UINT, WPARAM};
use winapi::shared::windef::{HBITMAP, HDC, HWND, POINT, RECT};
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::{GetCurrentProcessId, GetWindowThreadProcessId, OpenProcess};
use winapi::um::winbase::QueryFullProcessImageNameW;
use winapi::um::wingdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, SRCCOPY, SelectObject,
};
use winapi::um::winnt::{HANDLE, PROCESS_QUERY_LIMITED_INFORMATION};
use winapi::um::winuser::*;

use crate::backend::ComputerBackend;
use crate::encode::encode_rgba_png;
use crate::keymap::{KeyChord, Modifier, windows_vk_for_key};
use crate::types::{
    Bounds, Capture, ComputerError, DesktopInfo, DisplayInfo, FrameInfo, FrameTarget,
    InputStrategy, MouseButton, PlatformInfo, PointerTarget, WindowInfo,
};

const VIRTUAL_DISPLAY_ID: &str = "windows-virtual";
const PARKED_DESKTOP: &str = "parked";
const CURRENT_DESKTOP: &str = "current";
const WHEEL_DELTA: i32 = 120;
const WM_MOUSEHWHEEL: UINT = 0x020E;

#[derive(Clone, Copy)]
struct ParkedWindow {
    rect: RECT,
    was_zoomed: bool,
}

pub struct WindowsBackend {
    pid: u32,
    parked: Mutex<HashMap<isize, ParkedWindow>>,
    dragging: Mutex<Option<(isize, MouseButton)>>,
    isolation_active: Mutex<bool>,
}

impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            pid: unsafe { GetCurrentProcessId() },
            parked: Mutex::new(HashMap::new()),
            dragging: Mutex::new(None),
            isolation_active: Mutex::new(false),
        }
    }

    fn hwnd(&self, window_id: &str) -> Result<HWND, ComputerError> {
        let raw = window_id
            .trim_start_matches("0x")
            .parse::<usize>()
            .map_err(|_| ComputerError::Invalid(format!("invalid window id `{window_id}`")))?;
        let hwnd = raw as HWND;
        if hwnd.is_null() {
            return Err(ComputerError::Invalid("null window handle".to_string()));
        }
        Ok(hwnd)
    }

    fn window_id(hwnd: HWND) -> String {
        format!("0x{:x}", hwnd as usize)
    }

    fn virtual_screen(&self) -> Bounds {
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            Bounds::new(x, y, width as u32, height as u32)
        }
    }

    fn window_title(&self, hwnd: HWND) -> String {
        unsafe {
            let length = GetWindowTextLengthW(hwnd);
            if length <= 0 {
                return String::new();
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
            if copied <= 0 {
                return String::new();
            }
            String::from_utf16_lossy(&buffer[..copied as usize])
        }
    }

    fn window_pid(&self, hwnd: HWND) -> Option<u32> {
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            (pid != 0).then_some(pid)
        }
    }

    fn process_name(&self, pid: u32) -> String {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
            if process.is_null() {
                return String::new();
            }
            let mut buffer = vec![0u16; 2048];
            let mut length = buffer.len() as DWORD;
            let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length);
            CloseHandle(process);
            if ok == 0 {
                return String::new();
            }
            let path = OsString::from_wide(&buffer[..length as usize]);
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string())
        }
    }

    fn rect(&self, hwnd: HWND) -> Bounds {
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return Bounds::default();
            }
            Bounds::new(
                rect.left,
                rect.top,
                (rect.right - rect.left).max(0) as u32,
                (rect.bottom - rect.top).max(0) as u32,
            )
        }
    }

    fn client_bounds(&self, hwnd: HWND) -> Bounds {
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetClientRect(hwnd, &mut rect) == 0 {
                return Bounds::default();
            }
            let mut origin = POINT { x: 0, y: 0 };
            if ClientToScreen(hwnd, &mut origin) == 0 {
                return Bounds::default();
            }
            Bounds::new(
                origin.x,
                origin.y,
                (rect.right - rect.left).max(0) as u32,
                (rect.bottom - rect.top).max(0) as u32,
            )
        }
    }

    fn ex_style(&self, hwnd: HWND) -> u32 {
        unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 }
    }

    fn is_parked(&self, hwnd: HWND) -> bool {
        self.parked.lock().unwrap().contains_key(&(hwnd as isize))
    }

    fn info_for_hwnd(&self, hwnd: HWND) -> WindowInfo {
        let bounds = self.rect(hwnd);
        let client_bounds = self.client_bounds(hwnd);
        let pid = self.window_pid(hwnd);
        let active = unsafe { GetForegroundWindow() == hwnd };
        let minimized = unsafe { IsIconic(hwnd) != 0 };
        let desktop = if self.is_parked(hwnd) {
            PARKED_DESKTOP
        } else {
            CURRENT_DESKTOP
        };
        WindowInfo {
            id: Self::window_id(hwnd),
            title: self.window_title(hwnd),
            app: pid
                .map(|pid| self.process_name(pid))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "application".to_string()),
            pid,
            bounds,
            client_bounds,
            active,
            minimized,
            desktop_id: Some(desktop.to_string()),
            capturable: !minimized,
        }
    }

    fn all_windows(&self) -> Vec<HWND> {
        unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let windows = &mut *(lparam as *mut Vec<HWND>);
            windows.push(hwnd);
            TRUE
        }
        let mut windows: Vec<HWND> = Vec::new();
        unsafe {
            EnumWindows(Some(callback), &mut windows as *mut _ as LPARAM);
        }
        windows
    }

    fn should_list(&self, hwnd: HWND) -> bool {
        unsafe {
            if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                return false;
            }
        }
        if self.ex_style(hwnd) & WS_EX_TOOLWINDOW != 0 {
            return false;
        }
        if self.window_pid(hwnd) == Some(self.pid) {
            return false;
        }
        !self.window_title(hwnd).trim().is_empty()
    }

    fn focused_child(&self, hwnd: HWND) -> HWND {
        unsafe {
            let mut pid = 0u32;
            let thread = GetWindowThreadProcessId(hwnd, &mut pid);
            if thread == 0 {
                return hwnd;
            }
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut info) == 0 || info.hwndFocus.is_null() {
                hwnd
            } else {
                info.hwndFocus
            }
        }
    }

    fn activate(&self, hwnd: HWND) {
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            let foreground = GetForegroundWindow();
            let foreground_thread =
                GetWindowThreadProcessId(foreground, std::ptr::null_mut::<u32>());
            let mut pid = 0u32;
            let target_thread = GetWindowThreadProcessId(hwnd, &mut pid);
            if foreground_thread != 0 && target_thread != 0 && foreground_thread != target_thread {
                AttachThreadInput(foreground_thread, target_thread, TRUE);
                BringWindowToTop(hwnd);
                SetForegroundWindow(hwnd);
                SetFocus(hwnd);
                AttachThreadInput(foreground_thread, target_thread, FALSE);
            } else {
                BringWindowToTop(hwnd);
                SetForegroundWindow(hwnd);
                SetFocus(hwnd);
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }

    fn unpark_for_foreground(&self, hwnd: HWND) {
        let parked = self.parked.lock().unwrap().remove(&(hwnd as isize));
        if let Some(parked) = parked {
            unsafe {
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    parked.rect.left,
                    parked.rect.top,
                    parked.rect.right - parked.rect.left,
                    parked.rect.bottom - parked.rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
                if parked.was_zoomed {
                    ShowWindow(hwnd, SW_MAXIMIZE);
                }
            }
        }
    }

    fn park(&self, hwnd: HWND) -> Result<(), ComputerError> {
        if self.is_parked(hwnd) {
            return Ok(());
        }
        let rect = unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return Err(ComputerError::Backend(
                    "cannot read window rect".to_string(),
                ));
            }
            rect
        };
        let was_zoomed = unsafe { IsZoomed(hwnd) != 0 };
        let screen = self.virtual_screen();
        let off_x = screen.right() + 200;
        let off_y = screen.y;
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                off_x,
                off_y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
        if ok == 0 {
            return Err(ComputerError::Backend(
                "SetWindowPos could not move the window to the parking lane".to_string(),
            ));
        }
        self.parked
            .lock()
            .unwrap()
            .insert(hwnd as isize, ParkedWindow { rect, was_zoomed });
        Ok(())
    }

    fn restore(&self, hwnd: HWND) {
        let parked = self.parked.lock().unwrap().remove(&(hwnd as isize));
        if let Some(parked) = parked {
            unsafe {
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    parked.rect.left,
                    parked.rect.top,
                    parked.rect.right - parked.rect.left,
                    parked.rect.bottom - parked.rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
                if parked.was_zoomed {
                    ShowWindow(hwnd, SW_MAXIMIZE);
                }
            }
        }
    }

    fn client_to_screen(&self, hwnd: HWND, x: f64, y: f64) -> (i32, i32) {
        let mut point = POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        unsafe {
            if ClientToScreen(hwnd, &mut point) == 0 {
                (x.round() as i32, y.round() as i32)
            } else {
                (point.x, point.y)
            }
        }
    }

    fn capture_client(&self, hwnd: HWND, max_dim: Option<u32>) -> Result<Capture, ComputerError> {
        let info = self.info_for_hwnd(hwnd);
        let width = info.client_bounds.width as i32;
        let height = info.client_bounds.height as i32;
        if width <= 0 || height <= 0 {
            return Err(ComputerError::Backend(
                "the window has an empty client area".to_string(),
            ));
        }
        let rgba = unsafe { grab_window_rgba(hwnd, width, height)? };
        let (png, image_width, image_height) =
            encode_rgba_png(rgba, width as u32, height as u32, max_dim)?;
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target: FrameTarget::Window { window_id: info.id },
                image_width,
                image_height,
                native_width: width as u32,
                native_height: height as u32,
                origin_x: info.client_bounds.x,
                origin_y: info.client_bounds.y,
                desktop_id: info.desktop_id,
            },
        })
    }

    fn capture_screen_region(
        &self,
        bounds: Bounds,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let width = bounds.width as i32;
        let height = bounds.height as i32;
        if width <= 0 || height <= 0 {
            return Err(ComputerError::Backend(
                "the virtual screen has an empty size".to_string(),
            ));
        }
        let rgba = unsafe { grab_screen_rgba(bounds.x, bounds.y, width, height)? };
        let (png, image_width, image_height) =
            encode_rgba_png(rgba, width as u32, height as u32, max_dim)?;
        Ok(Capture {
            png,
            frame: FrameInfo {
                id: String::new(),
                target: FrameTarget::Screen {
                    display_id: Some(VIRTUAL_DISPLAY_ID.to_string()),
                },
                image_width,
                image_height,
                native_width: width as u32,
                native_height: height as u32,
                origin_x: bounds.x,
                origin_y: bounds.y,
                desktop_id: Some(CURRENT_DESKTOP.to_string()),
            },
        })
    }

    fn send_mouse_button(&self, button: MouseButton, pressed: bool) {
        let flag = match (button, pressed) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        unsafe {
            let mut input: INPUT = std::mem::zeroed();
            input.type_ = INPUT_MOUSE;
            input.u.mi_mut().dwFlags = flag;
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn send_key_vk(&self, vk: u16, pressed: bool) {
        unsafe {
            let mut input: INPUT = std::mem::zeroed();
            input.type_ = INPUT_KEYBOARD;
            input.u.ki_mut().wVk = vk;
            if !pressed {
                input.u.ki_mut().dwFlags |= KEYEVENTF_KEYUP;
            }
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn send_unicode(&self, unit: u16) {
        unsafe {
            let mut down: INPUT = std::mem::zeroed();
            down.type_ = INPUT_KEYBOARD;
            down.u.ki_mut().wScan = unit;
            down.u.ki_mut().dwFlags = KEYEVENTF_UNICODE;
            SendInput(1, &mut down, std::mem::size_of::<INPUT>() as i32);

            let mut up: INPUT = std::mem::zeroed();
            up.type_ = INPUT_KEYBOARD;
            up.u.ki_mut().wScan = unit;
            up.u.ki_mut().dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
            SendInput(1, &mut up, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn modifier_vk(modifier: Modifier) -> u16 {
        match modifier {
            Modifier::Ctrl => VK_CONTROL as u16,
            Modifier::Shift => VK_SHIFT as u16,
            Modifier::Alt => VK_MENU as u16,
            Modifier::Super => VK_LWIN as u16,
        }
    }

    fn key_vk(&self, key: &str) -> Result<(u16, bool), ComputerError> {
        if let Some(vk) = windows_vk_for_key(key) {
            return Ok((vk as u16, false));
        }
        let mut chars = key.chars();
        let ch = chars
            .next()
            .ok_or_else(|| ComputerError::Invalid(format!("unknown key `{key}`")))?;
        if chars.next().is_some() {
            return Err(ComputerError::Invalid(format!("unknown key `{key}`")));
        }
        unsafe {
            let scan = VkKeyScanW(ch as u16);
            if scan == -1 {
                return Err(ComputerError::Invalid(format!(
                    "key `{ch}` is not available on the current keyboard layout"
                )));
            }
            let vk = (scan as u16) & 0xff;
            let shift = (scan as u16) & 1 != 0;
            Ok((vk, shift))
        }
    }

    fn post_message(&self, hwnd: HWND, message: UINT, wparam: WPARAM, lparam: LPARAM) {
        unsafe {
            PostMessageW(hwnd, message, wparam, lparam);
        }
    }

    fn post_mouse(&self, hwnd: HWND, x: i32, y: i32, pressed: Option<MouseButton>) {
        let (message, wparam) = match pressed {
            Some(MouseButton::Left) => (WM_LBUTTONDOWN, MK_LBUTTON as WPARAM),
            Some(MouseButton::Right) => (WM_RBUTTONDOWN, MK_RBUTTON as WPARAM),
            Some(MouseButton::Middle) => (WM_MBUTTONDOWN, MK_MBUTTON as WPARAM),
            None => {
                let dragging = *self.dragging.lock().unwrap();
                let button = match dragging {
                    Some((_, MouseButton::Left)) => MK_LBUTTON,
                    Some((_, MouseButton::Right)) => MK_RBUTTON,
                    Some((_, MouseButton::Middle)) => MK_MBUTTON,
                    None => 0,
                };
                (WM_MOUSEMOVE, button as WPARAM)
            }
        };
        self.post_message(hwnd, message, wparam, make_lparam(x, y));
    }

    fn post_button_up(&self, hwnd: HWND, button: MouseButton, x: i32, y: i32) {
        let message = match button {
            MouseButton::Left => WM_LBUTTONUP,
            MouseButton::Right => WM_RBUTTONUP,
            MouseButton::Middle => WM_MBUTTONUP,
        };
        self.post_message(hwnd, message, 0, make_lparam(x, y));
    }

    fn post_text(&self, hwnd: HWND, text: &str) {
        let target = self.focused_child(hwnd);
        for unit in text.encode_utf16() {
            self.post_message(target, WM_CHAR, unit as WPARAM, 0);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    fn post_modifiers(&self, hwnd: HWND, modifiers: &[Modifier], down: bool) {
        let target = self.focused_child(hwnd);
        for modifier in modifiers {
            let vk = Self::modifier_vk(*modifier);
            self.post_message(
                target,
                if down { WM_KEYDOWN } else { WM_KEYUP },
                vk as WPARAM,
                0,
            );
        }
    }

    fn post_key(&self, hwnd: HWND, chord: &KeyChord) -> Result<(), ComputerError> {
        let target = self.focused_child(hwnd);
        self.post_modifiers(target, &chord.modifiers, true);
        let (vk, needs_shift) = self.key_vk(&chord.key)?;
        if needs_shift && !chord.modifiers.contains(&Modifier::Shift) {
            self.post_message(target, WM_KEYDOWN, VK_SHIFT as WPARAM, 0);
        }
        self.post_message(target, WM_KEYDOWN, vk as WPARAM, 0);
        self.post_message(target, WM_KEYUP, vk as WPARAM, 0);
        if needs_shift && !chord.modifiers.contains(&Modifier::Shift) {
            self.post_message(target, WM_KEYUP, VK_SHIFT as WPARAM, 0);
        }
        self.post_modifiers(target, &chord.modifiers, false);
        Ok(())
    }
}

unsafe fn grab_window_rgba(hwnd: HWND, width: i32, height: i32) -> Result<Vec<u8>, ComputerError> {
    let window_dc = unsafe { GetDC(hwnd) };
    if window_dc.is_null() {
        return Err(ComputerError::Backend("GetDC(window) failed".to_string()));
    }
    let memory_dc = unsafe { CreateCompatibleDC(window_dc) };
    if memory_dc.is_null() {
        unsafe { ReleaseDC(hwnd, window_dc) };
        return Err(ComputerError::Backend(
            "CreateCompatibleDC failed".to_string(),
        ));
    }
    let bitmap = unsafe { CreateCompatibleBitmap(window_dc, width, height) };
    if bitmap.is_null() {
        unsafe {
            DeleteDC(memory_dc);
            ReleaseDC(hwnd, window_dc);
        }
        return Err(ComputerError::Backend(
            "CreateCompatibleBitmap failed".to_string(),
        ));
    }
    let old = unsafe { SelectObject(memory_dc, bitmap as _) };
    let printed =
        unsafe { PrintWindow(hwnd, memory_dc, PW_CLIENTONLY | PW_RENDERFULLCONTENT) } != 0;
    if !printed {
        unsafe {
            BitBlt(memory_dc, 0, 0, width, height, window_dc, 0, 0, SRCCOPY);
        }
    }
    let pixels = unsafe { read_bitmap(memory_dc, bitmap, width, height) };
    unsafe {
        SelectObject(memory_dc, old);
        DeleteObject(bitmap as _);
        DeleteDC(memory_dc);
        ReleaseDC(hwnd, window_dc);
    }
    pixels
}

unsafe fn grab_screen_rgba(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, ComputerError> {
    let screen_dc = unsafe { GetDC(std::ptr::null_mut()) };
    if screen_dc.is_null() {
        return Err(ComputerError::Backend("GetDC(screen) failed".to_string()));
    }
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc.is_null() {
        unsafe { ReleaseDC(std::ptr::null_mut(), screen_dc) };
        return Err(ComputerError::Backend(
            "CreateCompatibleDC failed".to_string(),
        ));
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
    if bitmap.is_null() {
        unsafe {
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
        }
        return Err(ComputerError::Backend(
            "CreateCompatibleBitmap failed".to_string(),
        ));
    }
    let old = unsafe { SelectObject(memory_dc, bitmap as _) };
    unsafe {
        BitBlt(memory_dc, 0, 0, width, height, screen_dc, x, y, SRCCOPY);
    }
    let pixels = unsafe { read_bitmap(memory_dc, bitmap, width, height) };
    unsafe {
        SelectObject(memory_dc, old);
        DeleteObject(bitmap as _);
        DeleteDC(memory_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
    }
    pixels
}

unsafe fn read_bitmap(
    dc: HDC,
    bitmap: HBITMAP,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, ComputerError> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut buffer = vec![0u8; width as usize * height as usize * 4];
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            buffer.as_mut_ptr() as *mut _,
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return Err(ComputerError::Backend(
            "GetDIBits returned no scanlines".to_string(),
        ));
    }
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Ok(buffer)
}

fn make_lparam(x: i32, y: i32) -> LPARAM {
    (((y as u16 as u32) << 16) | (x as u16 as u32)) as LPARAM
}

#[async_trait]
impl ComputerBackend for WindowsBackend {
    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            platform: "windows".to_string(),
            display_server: "win32".to_string(),
            backend: "win32-gdi".to_string(),
            screen_capture: true,
            window_capture: true,
            window_listing: true,
            input: true,
            desktop_isolation: true,
            isolation_kind: Some("background-parking".to_string()),
            notes: vec![
                "Windows: PrintWindow/GDI 截图，SendInput 前台输入，后台停车位隔离。".to_string(),
                "忽略 WM_* 消息的应用请用 strategy=foreground；窗口会临时恢复并置前。".to_string(),
            ],
        }
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        let bounds = self.virtual_screen();
        Ok(vec![DisplayInfo {
            id: VIRTUAL_DISPLAY_ID.to_string(),
            name: "Windows 虚拟屏幕".to_string(),
            bounds,
            scale: 1.0,
            primary: true,
        }])
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        Ok(self
            .all_windows()
            .into_iter()
            .filter(|hwnd| self.should_list(*hwnd))
            .map(|hwnd| self.info_for_hwnd(hwnd))
            .collect())
    }

    async fn window_info(&self, window_id: &str) -> Result<WindowInfo, ComputerError> {
        let hwnd = self.hwnd(window_id)?;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return Err(ComputerError::NotFound(format!("window `{window_id}`")));
            }
        }
        Ok(self.info_for_hwnd(hwnd))
    }

    async fn capture_screen(
        &self,
        display: Option<&str>,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        if let Some(display) = display
            && display != VIRTUAL_DISPLAY_ID
        {
            return Err(ComputerError::NotFound(format!("display `{display}`")));
        }
        self.capture_screen_region(self.virtual_screen(), max_dim)
    }

    async fn capture_window(
        &self,
        window_id: &str,
        max_dim: Option<u32>,
    ) -> Result<Capture, ComputerError> {
        let hwnd = self.hwnd(window_id)?;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return Err(ComputerError::NotFound(format!("window `{window_id}`")));
            }
        }
        self.capture_client(hwnd, max_dim)
    }

    async fn pointer_move(&self, target: &PointerTarget) -> Result<(), ComputerError> {
        let Some(window_id) = &target.window_id else {
            unsafe {
                SetCursorPos(
                    target.screen_x.round() as i32,
                    target.screen_y.round() as i32,
                );
            }
            return Ok(());
        };
        let hwnd = self.hwnd(window_id)?;
        if target.background {
            let mut dragging = self.dragging.lock().unwrap();
            let pressed = dragging
                .as_ref()
                .map(|(id, button)| {
                    if *id == hwnd as isize {
                        Some(*button)
                    } else {
                        None
                    }
                })
                .flatten();
            drop(dragging);
            let wparam = match pressed {
                Some(MouseButton::Left) => MK_LBUTTON as WPARAM,
                Some(MouseButton::Right) => MK_RBUTTON as WPARAM,
                Some(MouseButton::Middle) => MK_MBUTTON as WPARAM,
                None => 0,
            };
            self.post_message(
                hwnd,
                WM_MOUSEMOVE,
                wparam,
                make_lparam(target.client_x as i32, target.client_y as i32),
            );
        } else {
            if self.is_parked(hwnd) {
                self.unpark_for_foreground(hwnd);
                self.activate(hwnd);
            }
            let (x, y) = self.client_to_screen(hwnd, target.client_x, target.client_y);
            unsafe {
                SetCursorPos(x, y);
            }
        }
        Ok(())
    }

    async fn pointer_button(
        &self,
        target: &PointerTarget,
        button: MouseButton,
        pressed: bool,
        strategy: InputStrategy,
        modifiers: &[Modifier],
    ) -> Result<(), ComputerError> {
        let Some(window_id) = &target.window_id else {
            if pressed {
                unsafe {
                    SetCursorPos(
                        target.screen_x.round() as i32,
                        target.screen_y.round() as i32,
                    );
                }
            }
            self.send_mouse_button(button, pressed);
            return Ok(());
        };
        let hwnd = self.hwnd(window_id)?;
        let background = target.background || strategy == InputStrategy::Background;
        if background {
            if !self.is_parked(hwnd) {
                return Err(ComputerError::Invalid(
                    "background input requires a parked window".to_string(),
                ));
            }
            let x = target.client_x as i32;
            let y = target.client_y as i32;
            if pressed {
                self.post_mouse(hwnd, x, y, None);
                self.post_modifiers(hwnd, modifiers, true);
                self.post_mouse(hwnd, x, y, Some(button));
                *self.dragging.lock().unwrap() = Some((hwnd as isize, button));
            } else {
                self.post_button_up(hwnd, button, x, y);
                self.post_modifiers(hwnd, modifiers, false);
                *self.dragging.lock().unwrap() = None;
            }
            return Ok(());
        }

        if self.is_parked(hwnd) {
            self.unpark_for_foreground(hwnd);
        }
        if pressed {
            self.activate(hwnd);
            for modifier in modifiers {
                self.send_key_vk(Self::modifier_vk(*modifier), true);
            }
            let (x, y) = self.client_to_screen(hwnd, target.client_x, target.client_y);
            unsafe {
                SetCursorPos(x, y);
            }
        }
        self.send_mouse_button(button, pressed);
        if !pressed {
            for modifier in modifiers.iter().rev() {
                self.send_key_vk(Self::modifier_vk(*modifier), false);
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
        let Some(window_id) = &target.window_id else {
            unsafe {
                SetCursorPos(
                    target.screen_x.round() as i32,
                    target.screen_y.round() as i32,
                );
            }
            self.send_wheel(dx, dy);
            return Ok(());
        };
        let hwnd = self.hwnd(window_id)?;
        let background = target.background || strategy == InputStrategy::Background;
        if background {
            let (sx, sy) = self.client_to_screen(hwnd, target.client_x, target.client_y);
            if dy != 0 {
                let delta = (-dy * WHEEL_DELTA) as i32;
                self.post_message(
                    hwnd,
                    WM_MOUSEWHEEL,
                    ((delta as u32) << 16) as WPARAM,
                    make_lparam(sx, sy),
                );
            }
            if dx != 0 {
                let delta = (dx * WHEEL_DELTA) as i32;
                self.post_message(
                    hwnd,
                    WM_MOUSEHWHEEL,
                    ((delta as u32) << 16) as WPARAM,
                    make_lparam(sx, sy),
                );
            }
            return Ok(());
        }
        if self.is_parked(hwnd) {
            self.unpark_for_foreground(hwnd);
        }
        self.activate(hwnd);
        let (x, y) = self.client_to_screen(hwnd, target.client_x, target.client_y);
        unsafe {
            SetCursorPos(x, y);
        }
        self.send_wheel(dx, dy);
        Ok(())
    }

    async fn type_text(
        &self,
        text: &str,
        window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        if let Some(window_id) = window_id {
            let hwnd = self.hwnd(window_id)?;
            if strategy == InputStrategy::Background {
                self.post_text(hwnd, text);
                return Ok(());
            }
            if self.is_parked(hwnd) {
                self.unpark_for_foreground(hwnd);
            }
            self.activate(hwnd);
        }
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                self.send_key_vk(VK_RETURN as u16, true);
                self.send_key_vk(VK_RETURN as u16, false);
            } else {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    self.send_unicode(*unit);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        Ok(())
    }

    async fn press_key(
        &self,
        chord: &KeyChord,
        repeat: u32,
        hold_ms: u64,
        window_id: Option<&str>,
        strategy: InputStrategy,
    ) -> Result<(), ComputerError> {
        if let Some(window_id) = window_id {
            let hwnd = self.hwnd(window_id)?;
            if strategy == InputStrategy::Background {
                for _ in 0..repeat.max(1) {
                    self.post_key(hwnd, chord)?;
                }
                return Ok(());
            }
            if self.is_parked(hwnd) {
                self.unpark_for_foreground(hwnd);
            }
            self.activate(hwnd);
        }
        let (vk, needs_shift) = self.key_vk(&chord.key)?;
        for _ in 0..repeat.max(1) {
            let mut shift = needs_shift && !chord.modifiers.contains(&Modifier::Shift);
            for modifier in &chord.modifiers {
                self.send_key_vk(Self::modifier_vk(*modifier), true);
            }
            if shift {
                self.send_key_vk(VK_SHIFT as u16, true);
            }
            self.send_key_vk(vk, true);
            if hold_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
            }
            self.send_key_vk(vk, false);
            if shift {
                self.send_key_vk(VK_SHIFT as u16, false);
                shift = false;
            }
            for modifier in chord.modifiers.iter().rev() {
                self.send_key_vk(Self::modifier_vk(*modifier), false);
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        Ok(())
    }

    async fn activate_window(&self, window_id: &str) -> Result<(), ComputerError> {
        let hwnd = self.hwnd(window_id)?;
        if self.is_parked(hwnd) {
            self.unpark_for_foreground(hwnd);
        }
        self.activate(hwnd);
        Ok(())
    }

    async fn list_desktops(&self) -> Result<Vec<DesktopInfo>, ComputerError> {
        let active = *self.isolation_active.lock().unwrap();
        Ok(vec![
            DesktopInfo {
                id: CURRENT_DESKTOP.to_string(),
                name: "当前桌面".to_string(),
                index: Some(0),
                active: !active,
                supported: true,
                kind: "current".to_string(),
            },
            DesktopInfo {
                id: PARKED_DESKTOP.to_string(),
                name: "EffiBuddy 后台工作区".to_string(),
                index: Some(1),
                active,
                supported: true,
                kind: "parking".to_string(),
            },
        ])
    }

    async fn lock_isolated_desktop(&self, name: &str) -> Result<DesktopInfo, ComputerError> {
        *self.isolation_active.lock().unwrap() = true;
        Ok(DesktopInfo {
            id: PARKED_DESKTOP.to_string(),
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
        if desktop_id != PARKED_DESKTOP {
            return Err(ComputerError::Unsupported(format!(
                "Windows parking lane only supports desktop `{PARKED_DESKTOP}`"
            )));
        }
        let hwnd = self.hwnd(window_id)?;
        self.park(hwnd)
    }

    async fn switch_desktop(&self, desktop_id: &str) -> Result<(), ComputerError> {
        if desktop_id != PARKED_DESKTOP && desktop_id != CURRENT_DESKTOP {
            return Err(ComputerError::NotFound(format!("desktop `{desktop_id}`")));
        }
        *self.isolation_active.lock().unwrap() = desktop_id == PARKED_DESKTOP;
        Ok(())
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), ComputerError> {
        let hwnd = self.hwnd(window_id)?;
        self.restore(hwnd);
        Ok(())
    }

    async fn end_isolation(&self) -> Result<(), ComputerError> {
        let parked: Vec<HWND> = self
            .parked
            .lock()
            .unwrap()
            .keys()
            .map(|id| *id as HWND)
            .collect();
        for hwnd in parked {
            self.restore(hwnd);
        }
        *self.isolation_active.lock().unwrap() = false;
        Ok(())
    }

    async fn pin_overlay(&self, title: &str) -> Result<(), ComputerError> {
        for hwnd in self.all_windows() {
            if self.window_pid(hwnd) != Some(self.pid) {
                continue;
            }
            if !self.window_title(hwnd).contains(title) {
                continue;
            }
            unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        Ok(())
    }

    fn supports_isolation(&self) -> bool {
        true
    }
}

impl WindowsBackend {
    fn send_wheel(&self, dx: i32, dy: i32) {
        if dy != 0 {
            unsafe {
                let mut input: INPUT = std::mem::zeroed();
                input.type_ = INPUT_MOUSE;
                input.u.mi_mut().mouseData = (-dy * WHEEL_DELTA) as u32;
                input.u.mi_mut().dwFlags = MOUSEEVENTF_WHEEL;
                SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
            }
        }
        if dx != 0 {
            unsafe {
                let mut input: INPUT = std::mem::zeroed();
                input.type_ = INPUT_MOUSE;
                input.u.mi_mut().mouseData = (dx * WHEEL_DELTA) as u32;
                input.u.mi_mut().dwFlags = MOUSEEVENTF_HWHEEL;
                SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}
