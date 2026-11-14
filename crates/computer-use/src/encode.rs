//! PNG encoding/decoding and model-friendly downscaling.

use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};

use crate::types::ComputerError;

pub const DEFAULT_MAX_DIM: u32 = 1280;
pub const MAX_MAX_DIM: u32 = 2048;

fn target_dimensions(width: u32, height: u32, max_dim: Option<u32>) -> (u32, u32) {
    let max_dim = max_dim
        .unwrap_or(DEFAULT_MAX_DIM)
        .clamp(256, MAX_MAX_DIM)
        .max(1);
    let longest = width.max(height);
    if longest <= max_dim || longest == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = max_dim as f64 / longest as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// Encode raw RGBA pixels as PNG, downscaling so the longest edge is at most
/// `max_dim` (default 1280, clamped to 256..=2048).
pub fn encode_rgba_png(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    max_dim: Option<u32>,
) -> Result<(Vec<u8>, u32, u32), ComputerError> {
    let image = RgbaImage::from_raw(width.max(1), height.max(1), rgba)
        .ok_or_else(|| ComputerError::Backend("invalid RGBA buffer length".to_string()))?;
    encode_image(image, max_dim)
}

fn encode_image(
    image: RgbaImage,
    max_dim: Option<u32>,
) -> Result<(Vec<u8>, u32, u32), ComputerError> {
    let (width, height) = image.dimensions();
    let (target_width, target_height) = target_dimensions(width, height, max_dim);
    let image = if (target_width, target_height) != (width, height) {
        image::imageops::resize(&image, target_width, target_height, FilterType::Triangle)
    } else {
        image
    };
    let (width, height) = image.dimensions();
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|error| ComputerError::Backend(format!("PNG encode failed: {error}")))?;
    Ok((png, width, height))
}

/// Decode a PNG (portal screenshots), optionally crop it to a monitor rectangle
/// in image pixels, then downscale/re-encode it for the model.
pub fn decode_crop_encode_png(
    bytes: &[u8],
    crop: Option<(i32, i32, u32, u32)>,
    max_dim: Option<u32>,
) -> Result<(Vec<u8>, u32, u32), ComputerError> {
    let image = image::load_from_memory(bytes)
        .map_err(|error| ComputerError::Backend(format!("PNG decode failed: {error}")))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    let image = if let Some((x, y, crop_width, crop_height)) = crop {
        let x = x.max(0) as u32;
        let y = y.max(0) as u32;
        let crop_width = crop_width.min(width.saturating_sub(x)).max(1);
        let crop_height = crop_height.min(height.saturating_sub(y)).max(1);
        image::imageops::crop_imm(&image, x, y, crop_width, crop_height).to_image()
    } else {
        image
    };
    encode_image(image, max_dim)
}

/// Decode a PNG without re-encoding (used by the CLI probe to save the raw
/// portal capture).
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::load_from_memory(bytes)
        .ok()
        .map(|image| (image.width(), image.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_long_edge_and_round_trips() {
        let width = 1920;
        let height = 1080;
        let rgba = vec![255_u8; width as usize * height as usize * 4];
        let (png, out_width, out_height) = encode_rgba_png(rgba, width, height, Some(640)).unwrap();
        assert_eq!(out_width, 640);
        assert_eq!(out_height, 360);
        assert!(png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn keeps_small_images_and_crops() {
        let width = 100;
        let height = 50;
        let rgba = vec![128_u8; width as usize * height as usize * 4];
        let (_, out_width, out_height) = encode_rgba_png(rgba, width, height, Some(1280)).unwrap();
        assert_eq!((out_width, out_height), (100, 50));
    }
}
