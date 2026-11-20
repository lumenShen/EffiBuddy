//! Image views and rounded-corner coverage. The color stage lives in `cpu.rs`
//! (folded into a matrix) and in `backdrop.wgsl` (folded the same way).

use crate::params::CornerRadius;

/// A borrowed, tightly-or-padded RGBA8 source image.
#[derive(Clone, Copy, Debug)]
pub struct ImageRef<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bytes per row. `width * 4` for tightly packed images.
    pub stride: usize,
}

impl<'a> ImageRef<'a> {
    pub fn new(data: &'a [u8], width: u32, height: u32) -> Self {
        Self {
            data,
            width,
            height,
            stride: width as usize * 4,
        }
    }

    pub fn with_stride(data: &'a [u8], width: u32, height: u32, stride: usize) -> Self {
        Self {
            data,
            width,
            height,
            stride,
        }
    }

    /// A view of one sub-rectangle of this image.
    pub fn region(&self, x: u32, y: u32, width: u32, height: u32) -> Self {
        let offset = y as usize * self.stride + x as usize * 4;
        Self {
            data: &self.data[offset..],
            width,
            height,
            stride: self.stride,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.data.is_empty()
    }
}

/// A tightly packed RGBA8 destination buffer.
#[derive(Debug)]
pub struct ImageMut<'a> {
    pub data: &'a mut [u8],
    pub width: u32,
    pub height: u32,
}

impl<'a> ImageMut<'a> {
    pub fn new(data: &'a mut [u8], width: u32, height: u32) -> Self {
        Self {
            data,
            width,
            height,
        }
    }

    pub fn expected_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Signed distance to the rounded rectangle, negative inside.
#[inline]
pub fn rounded_rect_sdf(
    px: f32,
    py: f32,
    width: f32,
    height: f32,
    corners: &CornerRadius,
) -> f32 {
    let half_w = width * 0.5;
    let half_h = height * 0.5;

    // Separate the X/Y terms so each corner can carry its own radius.
    let qx = px - half_w;
    let qy = py - half_h;

    // Negative quadrants select the matching corner: (-x,-y) = top-left, etc.
    let radius = if qx < 0.0 {
        if qy < 0.0 {
            corners.top_left
        } else {
            corners.bottom_left
        }
    } else if qy < 0.0 {
        corners.top_right
    } else {
        corners.bottom_right
    }
    .clamp(0.0, half_w.min(half_h).max(0.0));

    let bx = qx.abs() - (half_w - radius);
    let by = qy.abs() - (half_h - radius);
    let outside = (bx.max(0.0).powi(2) + by.max(0.0).powi(2)).sqrt();
    outside + bx.max(by).min(0.0) - radius
}

/// Anti-aliased coverage of the panel at a pixel center, in `0.0..=1.0`.
#[inline]
pub fn corner_coverage(
    px: f32,
    py: f32,
    width: f32,
    height: f32,
    corners: &CornerRadius,
    softness: f32,
) -> f32 {
    if corners.is_zero() {
        return 1.0;
    }
    let d = rounded_rect_sdf(px, py, width, height, corners);
    let softness = softness.max(0.01);
    ((softness * 0.5 - d) / softness).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_is_full_inside_and_zero_far_outside() {
        let corners = CornerRadius::all(12.0);
        assert_eq!(corner_coverage(50.0, 50.0, 100.0, 100.0, &corners, 1.0), 1.0);
        assert_eq!(
            corner_coverage(0.1, 0.1, 100.0, 100.0, &corners, 1.0),
            0.0,
            "a 12px radius cuts the (0,0) corner pixel away"
        );
        assert_eq!(
            corner_coverage(14.0, 14.0, 100.0, 100.0, &corners, 1.0),
            1.0,
            "inside the corner arc is fully covered"
        );
        assert_eq!(corner_coverage(50.0, 0.5, 100.0, 100.0, &corners, 1.0), 1.0);
        assert_eq!(corner_coverage(-3.0, 50.0, 100.0, 100.0, &corners, 1.0), 0.0);
        // Exactly on the corner arc center offset: half covered.
        let coverage = corner_coverage(0.0, 0.0, 100.0, 100.0, &CornerRadius::all(50.0), 1.0);
        assert!(coverage < 0.5, "sharp corner at radius=max is ~empty: {coverage}");
    }

    #[test]
    fn square_corners_are_always_covered() {
        let corners = CornerRadius::ZERO;
        assert_eq!(corner_coverage(0.0, 0.0, 10.0, 10.0, &corners, 1.0), 1.0);
        assert_eq!(corner_coverage(-5.0, 5.0, 10.0, 10.0, &corners, 1.0), 1.0);
    }

    #[test]
    fn sub_region_view_offsets_correctly() {
        let data: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        let image = ImageRef::new(&data, 4, 4);
        let sub = image.region(1, 1, 2, 2);
        assert_eq!(sub.width, 2);
        assert_eq!(sub.height, 2);
        assert_eq!(&sub.data[..4], &data[(4 + 1) * 4..(4 + 1) * 4 + 4]);
    }
}
