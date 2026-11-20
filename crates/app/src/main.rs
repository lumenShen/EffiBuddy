//! EffiBuddy desktop entry point.

// A console-subsystem binary gets a console window when launched from
// Explorer. Run as a GUI binary on Windows instead. Standard handles are
// still inherited, so `--version` and terminal-launched logging keep working.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod agents;
mod app;
mod attachments;
mod chat;
mod chat_agent;
#[cfg(test)]
mod chat_bench;
#[cfg(test)]
mod chat_scroll_tests;
#[cfg(test)]
mod chat_stream_stress;
#[cfg(test)]
mod sidebar_layout_tests;
mod checkpoint;
mod commands;
mod composer_menu;
mod computer;
mod config;
mod context_meter;
mod crash;
mod draft_store;
mod events;
mod file_menu;
mod graph;
mod memory;
mod mobile;
mod model_config;
mod model_picker;
mod nav_bench;
mod perf;
mod project_store;
mod session_export;
mod settings;
mod side_pane;
mod skills;
mod smooth;
// Stress lab — opt-in only (`--features stress`); compiled out of default builds.
mod store;
#[cfg(feature = "stress")]
mod stress;
mod subagent;
mod thumbs;
mod update;

use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size,
};

use crate::app::{
    AppView, CyclePermissionMode, FocusComposer, NewSession, OpenCommandPalette, StopRun,
    ToggleGraph, ToggleMobileLayout, ToggleSettings, ToggleSidebar, ToggleTheme, ZoomIn,
    ZoomOut, ZoomReset,
};
use crate::config::AppConfig;
use effibuddy_ui::{EffiBuddyAssets, install_keybindings};

fn main() {
    // Must run before GPUI spawns its worker pool: glibc's default arena count
    // grows with the number of cores, and with one arena per worker the freed
    // chunks of large temporary buffers are never reused by other threads
    // (~1 GB retained on a long session). See memory.rs.
    memory::configure_allocator();

    // Crash reporter mode: a separate process that survives whatever killed
    // the main one and shows the saved report. It must not install the panic
    // hook again, or a reporter failure would spawn reporters forever.
    if std::env::args().any(|argument| argument == crash::CRASH_REPORT_ARG) {
        match crash::crash_report_path() {
            Some(path) => crash::run_crash_report_window(path),
            None => {
                eprintln!("`{}` requires a crash log path", crash::CRASH_REPORT_ARG);
                std::process::exit(2);
            }
        }
        return;
    }

    // CI smoke test and packaging scripts read the version headlessly; this
    // must exit before any window/GPU initialization.
    if std::env::args().any(|argument| argument == "--version") {
        println!("effibuddy {}", crate::update::current_version());
        return;
    }
    if std::env::args().any(|argument| argument == "--computer-probe") {
        run_computer_probe();
        return;
    }
    // Reads the real clipboard once and exits: the only way to verify that the
    // platform backends actually surface a screenshot or a copied file, which
    // no unit test can do.
    if std::env::args().any(|argument| argument == "--clipboard-probe") {
        run_clipboard_probe();
        return;
    }

    // A UI-thread panic otherwise takes the window down without a trace when
    // the app was started from a launcher. Keep the log and leave a window.
    crash::install_panic_reporter();

    Application::new()
        .with_assets(EffiBuddyAssets)
        .run(|cx: &mut App| {
            install_keybindings(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewSession, Some("EffiBuddy")),
                KeyBinding::new("ctrl-shift-t", ToggleTheme, Some("EffiBuddy")),
                KeyBinding::new("ctrl-shift-s", ToggleSettings, Some("EffiBuddy")),
                KeyBinding::new("ctrl-b", ToggleSidebar, Some("EffiBuddy")),
                KeyBinding::new("ctrl-l", FocusComposer, Some("EffiBuddy")),
                KeyBinding::new("ctrl-k", OpenCommandPalette, Some("EffiBuddy")),
                KeyBinding::new("ctrl-shift-g", ToggleGraph, Some("EffiBuddy")),
                KeyBinding::new("ctrl-shift-m", ToggleMobileLayout, Some("EffiBuddy")),
                KeyBinding::new("shift-tab", CyclePermissionMode, Some("EffiBuddy")),
                KeyBinding::new("escape", StopRun, Some("EffiBuddy")),
                // Whole-app zoom; `=` and `+` cover both the unshifted and
                // shifted keysym, layouts differ on where `+` lives.
                KeyBinding::new("ctrl-=", ZoomIn, Some("EffiBuddy")),
                KeyBinding::new("ctrl-+", ZoomIn, Some("EffiBuddy")),
                KeyBinding::new("ctrl--", ZoomOut, Some("EffiBuddy")),
                KeyBinding::new("ctrl-0", ZoomReset, Some("EffiBuddy")),
            ]);

            let bounds = Bounds::centered(None, size(px(1280.0), px(840.0)), cx);
            // `--mobile` opens the phone shell; `--mobile-fullscreen` also takes
            // the whole display, which is how the shell is meant to feel on a
            // small screen and how screenshots are taken on Wayland (no window
            // stacking control there).
            let mobile_shell = std::env::args().any(|argument| argument == "--mobile")
                || std::env::args().any(|argument| argument == "--mobile-fullscreen")
                || AppConfig::load().ui_layout.is_mobile();
            let window_bounds = if std::env::args().any(|argument| argument == "--mobile-fullscreen")
            {
                WindowBounds::Fullscreen(bounds)
            } else if mobile_shell {
                WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(430.0), px(780.0)),
                    cx,
                ))
            } else {
                WindowBounds::Windowed(bounds)
            };
            let options = WindowOptions {
                window_bounds: Some(window_bounds),
                titlebar: None,
                window_min_size: Some(if mobile_shell {
                    size(px(340.0), px(480.0))
                } else {
                    size(px(980.0), px(640.0))
                }),
                ..Default::default()
            };

            let window = cx
                .open_window(options, |window, cx| {
                    let view = cx.new(AppView::new);
                    let focus = view.read(cx).composer_focus_handle(cx);
                    focus.focus(window);
                    view
                })
                .expect("failed to open the EffiBuddy window");

            let _ = window.update(cx, |view, window, cx| {
                // Custom drag-head, but the compositor still needs a title and
                // app id so Alt+Tab / taskbars can identify the window.
                window.set_window_title("EffiBuddy");
                window.set_app_id("effibuddy");
                window.activate_window();
                // Restore the persisted whole-app zoom before the first frame
                // the user interacts with; 1.0 skips the extra re-layout.
                let config = AppConfig::load();
                let ui_zoom = config.ui_zoom;
                if (ui_zoom - 1.0).abs() > 1e-4 {
                    window.set_ui_zoom(ui_zoom);
                }
                // Pace presentation to the display. The window backend already
                // defaults to vsync, so only an explicit opt-out costs a swap
                // chain reconfigure here. `EFFIBUDDY_VSYNC=0` wins over the
                // persisted setting: the performance lab needs an unlocked
                // run even when the user has vsync on.
                let lab_vsync = std::env::var("EFFIBUDDY_VSYNC").map_or(true, |value| value != "0");
                let vsync = config.vsync && lab_vsync;
                if !vsync {
                    window.set_vsync(false);
                }
                // Mobile-shell preview knobs: open the drawer or one settings
                // page at startup so screenshots can verify them on a Wayland
                // session where window focus (and therefore input) cannot be
                // handed to the app from a script.
                let args: Vec<String> = std::env::args().collect();
                let page_arg = args
                    .iter()
                    .find_map(|argument| argument.strip_prefix("--mobile-settings="))
                    .map(str::to_string);
                if args.iter().any(|argument| argument == "--mobile-drawer") {
                    view.mobile.drawer_open = true;
                    view.mobile.drawer_p = 1.0;
                }
                if args.iter().any(|argument| argument == "--mobile-settings")
                    || page_arg.is_some()
                {
                    view.settings_open = true;
                    view.mobile.page = match page_arg.as_deref() {
                        Some("appearance") => crate::mobile::pages::SettingsPage::Appearance,
                        Some("general") => crate::mobile::pages::SettingsPage::General,
                        Some("models") => crate::mobile::pages::SettingsPage::Models,
                        Some("about") => crate::mobile::pages::SettingsPage::About,
                        _ => crate::mobile::pages::SettingsPage::Root,
                    };
                    cx.notify();
                }
                // `--mobile-sheet=model|actions|session` seeds a bottom sheet
                // so its layout can be verified without input focus.
                if let Some(kind) = args
                    .iter()
                    .find_map(|argument| argument.strip_prefix("--mobile-sheet="))
                {
                    view.mobile.sheet = match kind {
                        "actions" => Some(crate::mobile::MobileSheet::Actions),
                        "session" => view
                            .sessions
                            .first()
                            .map(|summary| {
                                crate::mobile::MobileSheet::Session(summary.id.clone())
                            }),
                        _ => Some(crate::mobile::MobileSheet::Model),
                    };
                    view.mobile.sheet_p = 1.0;
                    cx.notify();
                }
                if std::env::args().any(|argument| argument == "--preview-composer") {
                    view.seed_composer_preview(window, cx);
                }
                if std::env::args().any(|argument| argument == "--preview-composer-menu") {
                    view.seed_composer_menu_preview(window, cx);
                }
                if std::env::args().any(|argument| argument == "--preview-computer") {
                    view.seed_computer_preview(window, cx);
                }
            });
            cx.activate(true);
        });
}

/// Headless-ish developer probe for the computer backend:
///
/// ```text
/// cargo run -p effibuddy-app -- --computer-probe status
/// cargo run -p effibuddy-app -- --computer-probe displays
/// cargo run -p effibuddy-app -- --computer-probe windows
/// cargo run -p effibuddy-app -- --computer-probe screenshot --out /tmp/shot.png
/// ```
///
/// On Wayland the first screenshot triggers the desktop portal permission
/// dialog; this bypasses the agent approval UI on purpose.
fn run_computer_probe() {
    let args: Vec<String> = std::env::args().collect();
    let action = args.get(2).map(String::as_str).unwrap_or("status");
    let out = args
        .iter()
        .position(|argument| argument == "--out")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "/tmp/effibuddy-computer.png".to_string());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("computer probe runtime");
    runtime.block_on(async move {
        let backend = effibuddy_computer::create_backend();
        match action {
            "status" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&backend.platform_info())
                        .unwrap_or_else(|_| "{}".to_string())
                );
            }
            "displays" => match backend.list_displays().await {
                Ok(displays) => println!(
                    "{}",
                    serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string())
                ),
                Err(error) => {
                    eprintln!("displays failed: {error}");
                    std::process::exit(2);
                }
            },
            "windows" => match backend.list_windows().await {
                Ok(windows) => println!(
                    "{}",
                    serde_json::to_string_pretty(&windows).unwrap_or_else(|_| "[]".to_string())
                ),
                Err(error) => {
                    eprintln!("windows failed: {error}");
                    std::process::exit(2);
                }
            },
            "screenshot" => match backend.capture_screen(None, Some(1280)).await {
                Ok(capture) => {
                    if let Err(error) = std::fs::write(&out, &capture.png) {
                        eprintln!("cannot write {}: {error}", out);
                        std::process::exit(2);
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "saved": out,
                            "frame": capture.frame,
                            "bytes": capture.png.len(),
                        }))
                        .unwrap_or_else(|_| "{}".to_string())
                    );
                }
                Err(error) => {
                    eprintln!("screenshot failed: {error}");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("unknown probe action `{other}`; use status, windows or screenshot");
                std::process::exit(2);
            }
        }
    });
}

/// Developer probe: print what the platform reports for the current clipboard.
///
/// ```text
/// cargo run -p effibuddy-app -- --clipboard-probe
/// ```
///
/// Verifies the paste path end to end: copy a screenshot (or a file in your
/// file manager) and this prints `kind: image` / `kind: paths` with the shape
/// the composer will receive. Text copies print `kind: text` plus a preview.
fn run_clipboard_probe() {
    // Wayland only reports clipboard contents to a client that owns a focused
    // toplevel (`wl_data_device.selection` follows keyboard focus), so the
    // probe has to open a real window and wait a moment before reading.
    gpui::Application::new().run(|cx: &mut gpui::App| {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(360.0), px(240.0)),
                cx,
            ))),
            window_decorations: Some(gpui::WindowDecorations::Client),
            ..Default::default()
        };
        let window = cx
            .open_window(options, |_, cx| cx.new(|_| ClipboardProbe))
            .expect("failed to open the clipboard probe window");
        let _ = window.update(cx, |_, window, _| {
            window.set_window_title("EffiBuddy clipboard probe");
            window.activate_window();
        });

        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(900))
                .await;
            let _ = cx.update(|cx| {
                let report = clipboard_report(cx);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
                );
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            });
        })
        .detach();
    });
}

/// A blank window: the Wayland backend needs a focused toplevel before the
/// compositor sends it the clipboard selection.
struct ClipboardProbe;

impl gpui::Render for ClipboardProbe {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

/// Describe the current clipboard as the composer would see it.
fn clipboard_report(cx: &mut gpui::App) -> serde_json::Value {
    let Some(item) = cx.read_from_clipboard() else {
        return serde_json::json!({ "kind": "none" });
    };
    if let Some(image) = item.first_image() {
        return serde_json::json!({
            "kind": "image",
            "mime_type": image.format.mime_type(),
            "bytes": image.bytes.len(),
        });
    }
    if let Some(paths) = item.paths() {
        return serde_json::json!({
            "kind": "paths",
            "count": paths.paths().len(),
            "paths": paths
                .paths()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        });
    }
    if let Some(text) = item.text() {
        let preview: String = text.chars().take(120).collect();
        return serde_json::json!({
            "kind": "text",
            "chars": text.chars().count(),
            "preview": preview,
        });
    }
    serde_json::json!({ "kind": "empty" })
}
