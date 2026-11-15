//! EffiBuddy desktop entry point.

mod agents;
mod app;
mod attachments;
mod chat;
mod chat_agent;
mod checkpoint;
mod composer_menu;
mod computer;
mod config;
mod context_meter;
mod draft_store;
mod events;
mod file_menu;
mod graph;
mod model_config;
mod model_picker;
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
mod update;

use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size,
};

use crate::app::{
    AppView, CyclePermissionMode, FocusComposer, NewSession, OpenCommandPalette, StopRun,
    ToggleGraph, ToggleSettings, ToggleSidebar, ToggleTheme, ZoomIn, ZoomOut, ZoomReset,
};
use crate::config::AppConfig;
use effibuddy_ui::{EffiBuddyAssets, install_keybindings};

fn main() {
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
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                window_min_size: Some(size(px(980.0), px(640.0))),
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
                let ui_zoom = AppConfig::load().ui_zoom;
                if (ui_zoom - 1.0).abs() > 1e-4 {
                    window.set_ui_zoom(ui_zoom);
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
/// dialog; this bypasses the agent/consent UI on purpose.
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
