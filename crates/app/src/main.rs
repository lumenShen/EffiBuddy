//! EffiBuddy desktop entry point.

mod agents;
mod app;
mod chat;
mod chat_agent;
mod config;
mod events;
mod graph;
mod model_config;
mod model_picker;
mod perf;
mod settings;
mod side_pane;
mod smooth;
mod store;
mod subagent;

use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size,
};

use crate::app::{
    AppView, CyclePermissionMode, FocusComposer, NewSession, OpenCommandPalette, StopRun,
    ToggleGraph, ToggleSettings, ToggleSidebar, ToggleTheme,
};
use effibuddy_ui::{EffiBuddyAssets, install_keybindings};

fn main() {
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
                if std::env::args().any(|argument| argument == "--preview-composer") {
                    view.seed_composer_preview(window, cx);
                }
            });
            cx.activate(true);
        });
}
