//! Raster bookkeeping and image-pixel → native-coordinate mapping.
//!
//! A model only ever sees a (possibly scaled) PNG. Every action must carry the
//! `frame_id` of the raster it was derived from, so clicks cannot silently land
//! on a different layout after a window moved or a new capture replaced the
//! old one.

use std::collections::VecDeque;

use crate::types::{ComputerError, FrameInfo, PointerTarget};

/// Keep a small window of recent frames so a model can batch a few actions
/// against one screenshot, while old geometry is eventually evicted.
pub const MAX_FRAMES: usize = 8;

#[derive(Debug, Default)]
pub struct FrameRegistry {
    frames: VecDeque<FrameInfo>,
    counter: u64,
}

impl FrameRegistry {
    pub fn record(&mut self, mut frame: FrameInfo) -> FrameInfo {
        self.counter = self.counter.wrapping_add(1);
        if frame.id.is_empty() {
            frame.id = format!("f{}", self.counter);
        }
        self.frames.push_back(frame.clone());
        while self.frames.len() > MAX_FRAMES {
            self.frames.pop_front();
        }
        frame
    }

    pub fn resolve(&self, frame_id: Option<&str>) -> Result<FrameInfo, ComputerError> {
        match frame_id {
            Some(id) if !id.trim().is_empty() => self
                .frames
                .iter()
                .rev()
                .find(|frame| frame.id == id)
                .cloned()
                .ok_or_else(|| {
                    ComputerError::StaleFrame(format!(
                        "frame `{id}` is no longer available; take a new screenshot"
                    ))
                }),
            _ => self.frames.back().cloned().ok_or_else(|| {
                ComputerError::StaleFrame("no screenshot has been taken yet".into())
            }),
        }
    }

    pub fn latest(&self) -> Option<&FrameInfo> {
        self.frames.back()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

/// Map a pixel in the raster to global/client native coordinates.
pub fn map_frame(frame: &FrameInfo, x: f64, y: f64) -> Result<PointerTarget, ComputerError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(ComputerError::Invalid(
            "coordinates must be finite numbers".into(),
        ));
    }
    if x < 0.0 || y < 0.0 || x >= frame.image_width as f64 || y >= frame.image_height as f64 {
        return Err(ComputerError::OutOfBounds);
    }

    let native_x = x * frame.scale_x();
    let native_y = y * frame.scale_y();
    Ok(PointerTarget {
        screen_x: frame.origin_x as f64 + native_x,
        screen_y: frame.origin_y as f64 + native_y,
        client_x: native_x,
        client_y: native_y,
        window_id: frame.window_id().map(str::to_string),
        background: false,
        desktop_id: frame.desktop_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FrameInfo, FrameTarget};

    fn frame(id: &str, image: (u32, u32), native: (u32, u32), origin: (i32, i32)) -> FrameInfo {
        FrameInfo {
            id: id.to_string(),
            target: FrameTarget::Window {
                window_id: "0x1".to_string(),
            },
            image_width: image.0,
            image_height: image.1,
            native_width: native.0,
            native_height: native.1,
            origin_x: origin.0,
            origin_y: origin.1,
            desktop_id: None,
        }
    }

    #[test]
    fn maps_scaled_window_pixels_to_screen_and_client_coordinates() {
        let frame = frame("f1", (640, 400), (1920, 1200), (100, 50));
        let target = map_frame(&frame, 320.0, 200.0).unwrap();
        assert_eq!(target.screen_x, 1060.0);
        assert_eq!(target.screen_y, 650.0);
        assert_eq!(target.client_x, 960.0);
        assert_eq!(target.client_y, 600.0);
        assert_eq!(target.window_id.as_deref(), Some("0x1"));
    }

    #[test]
    fn rejects_out_of_bounds_pixels() {
        let frame = frame("f1", (640, 400), (640, 400), (0, 0));
        assert_eq!(
            map_frame(&frame, -1.0, 0.0),
            Err(ComputerError::OutOfBounds)
        );
        assert_eq!(
            map_frame(&frame, 0.0, 400.0),
            Err(ComputerError::OutOfBounds)
        );
        assert_eq!(
            map_frame(&frame, 640.0, 0.0),
            Err(ComputerError::OutOfBounds)
        );
        assert_eq!(
            map_frame(&frame, f64::NAN, 0.0),
            Err(ComputerError::Invalid(
                "coordinates must be finite numbers".into()
            ))
        );
    }

    #[test]
    fn registry_keeps_latest_frames_and_rejects_evicted_ids() {
        let mut registry = FrameRegistry::default();
        for index in 0..(MAX_FRAMES + 2) {
            registry.record(frame(&format!("f{index}"), (10, 10), (10, 10), (0, 0)));
        }
        assert_eq!(registry.len(), MAX_FRAMES);
        assert!(registry.resolve(Some("f0")).is_err());
        assert!(registry.resolve(Some("f9")).is_ok());
        assert_eq!(registry.resolve(None).unwrap().id, "f9");
    }

    #[test]
    fn missing_latest_frame_is_a_stale_state() {
        let registry = FrameRegistry::default();
        assert!(matches!(
            registry.resolve(None),
            Err(ComputerError::StaleFrame(_))
        ));
    }
}
