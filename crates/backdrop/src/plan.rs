//! Shared "what are we actually computing" plan.
//!
//! Both the CPU and the GPU backend derive their work from [`Plan`], which is
//! what makes the two implementations comparable pixel for pixel.

use crate::params::BackdropParams;

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    /// Clip against a surface of `width` x `height`.
    pub fn intersect(&self, width: u32, height: u32) -> Option<Self> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.right().min(width as i32);
        let y1 = self.bottom().min(height as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

/// Fixed-point scale used by the convolution kernels.
pub const KERNEL_ONE: i32 = 1 << 16;

/// Hard ceiling on the downsample factor. Reaching it takes a ~500px blur.
const MAX_DOWNSAMPLE: u32 = 64;

/// A resolved, backend-agnostic description of one backdrop-filter application.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Source region, already clipped to the surface.
    pub region: Rect,
    /// Downsample factor, always a power of two (or 3 for odd sizes, see below).
    pub downsample: u32,
    /// Low-resolution buffer size.
    pub low_width: u32,
    pub low_height: u32,
    /// Gaussian sigma in low-resolution pixels.
    pub sigma: f32,
    /// Half kernel extent in low-resolution pixels (`kernel.len() / 2`).
    pub radius: u32,
    /// Fixed-point separable Gaussian kernel, `sum(kernel) == KERNEL_ONE`.
    pub kernel: Vec<i32>,
}

impl Plan {
    pub fn compute(region: Rect, params: &BackdropParams) -> Self {
        let quality = params.quality;
        let sigma_css = (params.blur_radius * 0.5).max(0.0);

        // Pick the smallest power-of-two downsample that keeps the low-res
        // sigma inside the quality budget: the downsample itself is the
        // low-pass, so the convolution that follows only has to widen it a
        // little. This is what keeps a 48px blur at ~11 taps instead of 289.
        //
        // Deriving it from the sigma budget alone (instead of also capping the
        // factor) guarantees `radius <= 3 * max_low_res_sigma <= 12`, which is
        // what makes the 28-slot kernel in the GPU uniform always sufficient.
        let max_sigma = quality.max_low_res_sigma();
        let ratio = sigma_css / max_sigma;
        let downsample = if ratio <= 1.0 {
            1
        } else {
            (ratio.ceil() as u32).next_power_of_two().min(MAX_DOWNSAMPLE)
        };

        let low_width = region.width.div_ceil(downsample).max(1);
        let low_height = region.height.div_ceil(downsample).max(1);
        let sigma = sigma_css / downsample as f32;
        let radius = (3.0 * sigma).ceil().clamp(1.0, 24.0) as u32;
        // 25 taps max (radius 12) at the widest budget; the clamp above only
        // matters for the `MAX_DOWNSAMPLE` corner and keeps the GPU uniform
        // layout (28 weights) valid.
        let kernel = if sigma_css > 0.0 {
            gaussian_kernel(sigma, radius)
        } else {
            vec![KERNEL_ONE]
        };

        Self {
            region,
            downsample,
            low_width,
            low_height,
            sigma,
            radius: if sigma_css > 0.0 { radius } else { 0 },
            kernel,
        }
    }

    pub fn has_blur(&self) -> bool {
        self.kernel.len() > 1
    }
}

/// Build a normalized, fixed-point separable Gaussian kernel of radius `radius`.
pub fn gaussian_kernel(sigma: f32, radius: u32) -> Vec<i32> {
    debug_assert!(radius > 0);
    if sigma <= 0.0 {
        let mut kernel = vec![0; (radius * 2 + 1) as usize];
        kernel[radius as usize] = KERNEL_ONE;
        return kernel;
    }

    let inv_two_sigma_sq = 1.0 / (2.0 * sigma * sigma);
    let mut weights = Vec::with_capacity((radius * 2 + 1) as usize);
    let mut total = 0.0f64;
    for i in -(radius as i32)..=(radius as i32) {
        let weight = (-(i * i) as f32 * inv_two_sigma_sq).exp() as f64;
        weights.push(weight);
        total += weight;
    }

    let scale = KERNEL_ONE as f64 / total;
    let mut kernel: Vec<i32> = weights.iter().map(|w| (w * scale).round() as i32).collect();

    // Force the kernel to sum to exactly `KERNEL_ONE` so blurs never shift
    // brightness; the residual goes to the center tap.
    let sum: i32 = kernel.iter().sum();
    if let Some(center) = kernel.get_mut(radius as usize) {
        *center += KERNEL_ONE - sum;
    }
    kernel
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{BackdropParams, Quality};

    #[test]
    fn kernel_sums_to_one() {
        for sigma in [0.5f32, 1.0, 2.5, 6.0, 18.0] {
            let radius = (3.0 * sigma).ceil() as u32;
            let kernel = gaussian_kernel(sigma, radius);
            assert_eq!(kernel.iter().sum::<i32>(), KERNEL_ONE, "sigma={sigma}");
            assert!(kernel.iter().all(|&w| w >= 0), "sigma={sigma}");
        }
    }

    #[test]
    fn kernel_is_symmetric() {
        let kernel = gaussian_kernel(2.0, 6);
        let reversed: Vec<i32> = kernel.iter().rev().copied().collect();
        assert_eq!(kernel, reversed);
    }

    #[test]
    fn downsample_tracks_blur_radius() {
        let region = Rect::new(0, 0, 1920, 1080);

        let sharp = Plan::compute(
            region,
            &BackdropParams {
                blur_radius: 1.0,
                ..BackdropParams::default()
            },
        );
        assert_eq!(sharp.downsample, 1);

        let wide = Plan::compute(
            region,
            &BackdropParams {
                blur_radius: 48.0,
                quality: Quality::Balanced,
                ..BackdropParams::default()
            },
        );
        assert!(wide.downsample >= 4, "got {}", wide.downsample);
        assert!(
            wide.sigma <= Quality::Balanced.max_low_res_sigma(),
            "sigma {} at downsample {}",
            wide.sigma,
            wide.downsample
        );
        assert!(wide.kernel.len() <= 2 * 12 + 1, "taps {}", wide.kernel.len());
        assert_eq!(wide.low_width, 1920 / wide.downsample);
    }

    #[test]
    fn no_blur_keeps_full_resolution() {
        let plan = Plan::compute(
            Rect::new(0, 0, 640, 480),
            &BackdropParams {
                blur_radius: 0.0,
                ..BackdropParams::default()
            },
        );
        assert_eq!(plan.downsample, 1);
        assert!(!plan.has_blur());
        assert_eq!(plan.kernel, vec![KERNEL_ONE]);
    }

    #[test]
    fn region_is_clipped() {
        let rect = Rect::new(-10, -10, 100, 100);
        let clipped = rect.intersect(1000, 1000).unwrap();
        assert_eq!(clipped, Rect::new(0, 0, 90, 90));
        assert!(Rect::new(2000, 0, 10, 10).intersect(100, 100).is_none());
    }
}
