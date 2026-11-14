pub mod wayland;
pub mod x11;

use std::sync::Arc;

use crate::backend::{ComputerBackend, UnsupportedBackend};

/// Choose X11 vs Wayland once per process. `EFFIBUDDY_COMPUTER_BACKEND`
/// overrides detection (`x11` or `wayland`) for development and testing.
pub fn create_backend() -> Arc<dyn ComputerBackend> {
    let forced = std::env::var("EFFIBUDDY_COMPUTER_BACKEND")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase());

    if forced.as_deref() == Some("wayland") {
        return Arc::new(wayland::WaylandBackend::new());
    }
    if forced.as_deref() == Some("x11") {
        return x11_or_unsupported();
    }

    let wayland_session = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);
    if wayland_session {
        return Arc::new(wayland::WaylandBackend::new());
    }
    if std::env::var_os("DISPLAY").is_some() {
        return x11_or_unsupported();
    }

    Arc::new(UnsupportedBackend::new(
        "no DISPLAY or WAYLAND_DISPLAY is set; start EffiBuddy inside a graphical session",
    ))
}

fn x11_or_unsupported() -> Arc<dyn ComputerBackend> {
    match x11::X11Backend::new() {
        Ok(backend) => Arc::new(backend),
        Err(error) => Arc::new(UnsupportedBackend::new(format!(
            "cannot connect to the X11 display: {error}"
        ))),
    }
}
