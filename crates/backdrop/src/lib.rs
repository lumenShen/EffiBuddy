//! Backdrop filter for EffiBuddy: blur what is behind a translucent panel,
//! then tint and round its corners — the CPU/GPU equivalent of CSS
//! `backdrop-filter`.
//!
//! ```no_run
//! use effibuddy_backdrop::{BackdropFilter, BackdropParams, CornerRadius, ImageRef, Rect};
//!
//! let src = vec![128u8; 1920 * 1080 * 4];
//! let src = ImageRef::new(&src, 1920, 1080);
//! let region = Rect::new(560, 240, 800, 600);
//! let params = BackdropParams {
//!     blur_radius: 24.0,
//!     corners: CornerRadius::all(14.0),
//!     ..BackdropParams::default()
//! };
//!
//! let mut filter = BackdropFilter::new();
//! let mut out = vec![0u8; region.width as usize * region.height as usize * 4];
//! filter.apply(&src, region, &params, &mut out).unwrap();
//! ```
//!
//! # Why this shape
//!
//! The filter runs wide blurs at a reduced resolution (see [`plan::Plan`]) and
//! fuses the upsample, the color stage and the corner mask into a single write
//! pass, so the whole effect costs one read of the source region and one write
//! of the output region. That is what makes it viable at interactive rates
//! without a render pass of its own.
//!
//! # Backends
//!
//! * [`BackendKind::Cpu`] — rayon over row bands, no GPU needed, correct
//!   everywhere (including headless CI).
//! * [`BackendKind::Gpu`] — Vulkan compute through the same `blade-graphics`
//!   device family GPUI renders with. Results are read back to host memory;
//!   for a source that already lives on the GPU this is the fast path.
//!
//! [`BackdropFilter::new`] tries the GPU first and silently falls back to the
//! CPU, so callers never have to branch on hardware.

mod error;
pub mod image;
pub mod params;
pub mod plan;

mod cpu;

use cpu::CpuBackend;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod gpu;

pub use error::Error;
pub use image::{ImageMut, ImageRef};
pub use params::{BackdropParams, CornerRadius, Quality, Rgba8};
pub use plan::{KERNEL_ONE, Plan, Rect, gaussian_kernel};

use std::time::{Duration, Instant};

/// Which backend produced the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Cpu,
    Gpu,
}

/// Per-frame bookkeeping, useful for the performance lab and for deciding when
/// to downgrade [`Quality`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub backend: Option<BackendKind>,
    /// Wall-clock time of the last [`BackdropFilter::apply`] call.
    pub duration: Duration,
    pub bytes_read: usize,
    pub bytes_written: usize,
}

impl FrameStats {
    /// Throughput in bytes per second for the last frame.
    pub fn bandwidth(&self) -> f64 {
        let seconds = self.duration.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        (self.bytes_read + self.bytes_written) as f64 / seconds
    }

    /// Frames per second implied by the last frame.
    pub fn fps(&self) -> f64 {
        let seconds = self.duration.as_secs_f64();
        if seconds <= 0.0 {
            f64::INFINITY
        } else {
            1.0 / seconds
        }
    }
}

/// Keeps the effect inside a frame budget by degrading [`Quality`] when the
/// machine cannot keep up.
///
/// The filter is fast enough that this never triggers on the development
/// machine, but a 4K panel on an old iGPU is a different story, and a UI must
/// not drop frames because a blur is expensive. The downgrade is deliberately
/// sticky: once the effect is on the cheap path it stays there for a while
/// (`RESTORE_AFTER_FRAMES` comfortable frames) instead of oscillating.
#[derive(Clone, Copy, Debug)]
struct AdaptiveQuality {
    budget: Option<Duration>,
    /// Smoothed frame time in microseconds.
    ewma_us: f64,
    downgraded: bool,
    over_budget_frames: u32,
    comfortable_frames: u32,
}

impl Default for AdaptiveQuality {
    fn default() -> Self {
        Self {
            budget: None,
            ewma_us: 0.0,
            downgraded: false,
            over_budget_frames: 0,
            comfortable_frames: 0,
        }
    }
}

impl AdaptiveQuality {
    /// Frames over budget before dropping to [`Quality::Fast`].
    const DOWNGRADE_AFTER_FRAMES: u32 = 3;
    /// Consecutive comfortable frames before restoring the requested quality.
    const RESTORE_AFTER_FRAMES: u32 = 120;

    fn observe(&mut self, duration: Duration) {
        let Some(budget) = self.budget else {
            return;
        };
        let sample_us = duration.as_secs_f64() * 1e6;
        self.ewma_us = if self.ewma_us == 0.0 {
            sample_us
        } else {
            self.ewma_us * 0.8 + sample_us * 0.2
        };
        let budget_us = budget.as_secs_f64() * 1e6;

        if self.ewma_us > budget_us * 1.1 {
            self.over_budget_frames += 1;
            self.comfortable_frames = 0;
        } else {
            self.over_budget_frames = 0;
            if self.ewma_us < budget_us * 0.5 {
                self.comfortable_frames += 1;
            } else {
                self.comfortable_frames = 0;
            }
        }

        if !self.downgraded && self.over_budget_frames >= Self::DOWNGRADE_AFTER_FRAMES {
            self.downgraded = true;
            self.comfortable_frames = 0;
        } else if self.downgraded && self.comfortable_frames >= Self::RESTORE_AFTER_FRAMES {
            self.downgraded = false;
            self.over_budget_frames = 0;
        }
    }

    fn apply(&self, requested: Quality) -> Quality {
        if self.downgraded {
            Quality::Fast
        } else {
            requested
        }
    }
}

/// A reusable backdrop filter pipeline.
///
/// Owns its scratch buffers (CPU) or device resources (GPU); a steady-state
/// frame performs no allocation.
pub struct BackdropFilter {
    cpu: CpuBackend,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    gpu: Option<gpu::GpuBackend>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    gpu_failed: bool,
    prefer_gpu: bool,
    adaptive: AdaptiveQuality,
    stats: FrameStats,
}

impl Default for BackdropFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl BackdropFilter {
    /// Create a filter that prefers the GPU backend when one is usable.
    pub fn new() -> Self {
        Self::with_gpu(true)
    }

    /// Create a filter, optionally skipping GPU probing entirely.
    pub fn with_gpu(use_gpu: bool) -> Self {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let gpu = if use_gpu {
            gpu::GpuBackend::new().ok()
        } else {
            None
        };
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let gpu_is_none = gpu.is_none();

        Self {
            cpu: CpuBackend::new(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            gpu,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            gpu_failed: use_gpu && gpu_is_none,
            prefer_gpu: use_gpu,
            adaptive: AdaptiveQuality::default(),
            stats: FrameStats::default(),
        }
    }

    /// Ask the filter to keep each frame inside `budget`, downgrading
    /// [`Quality::Balanced`] / [`Quality::High`] to [`Quality::Fast`] when the
    /// smoothed frame time keeps exceeding it. `None` disables adaptation.
    ///
    /// Pass `Some(Duration::from_secs_f64(1.0 / 300.0))` to hold a 300 fps
    /// budget on weaker hardware.
    pub fn set_frame_budget(&mut self, budget: Option<Duration>) {
        self.adaptive = AdaptiveQuality {
            budget,
            ..AdaptiveQuality::default()
        };
    }

    /// The quality the next frame will actually run at, given the caller's
    /// request and the current frame budget pressure.
    pub fn effective_quality(&self, requested: Quality) -> Quality {
        self.adaptive.apply(requested)
    }

    /// `true` when the budget forced a downgrade.
    pub fn is_downgraded(&self) -> bool {
        self.adaptive.downgraded
    }

    /// Force a backend for all subsequent frames.
    pub fn set_prefer_gpu(&mut self, prefer_gpu: bool) {
        self.prefer_gpu = prefer_gpu;
    }

    /// The backend that will be used for the next frame.
    pub fn backend(&self) -> BackendKind {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if self.prefer_gpu
            && !self.gpu_failed
            && self.gpu.as_ref().is_some_and(|gpu| gpu.is_usable())
        {
            return BackendKind::Gpu;
        }
        BackendKind::Cpu
    }

    /// A short description of the active accelerator, for logs and the
    /// performance lab.
    pub fn accelerator(&self) -> String {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(gpu) = &self.gpu
            && self.prefer_gpu
            && gpu.is_usable()
        {
            return gpu.describe();
        }
        "cpu (rayon)".to_owned()
    }

    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    /// Run the filter over `region` of `src`, writing a tightly packed RGBA8
    /// image of exactly `region.width * region.height * 4` bytes into `out`.
    pub fn apply(
        &mut self,
        src: &ImageRef<'_>,
        region: Rect,
        params: &BackdropParams,
        out: &mut [u8],
    ) -> Result<FrameStats, Error> {
        let started = Instant::now();
        let Some(clipped) = region.intersect(src.width, src.height) else {
            return Err(Error::EmptyRegion);
        };
        if clipped != region {
            return Err(Error::SourceTooSmall);
        }

        let mut effective_params = *params;
        effective_params.quality = self.effective_quality(params.quality);
        let params = &effective_params;

        let plan = Plan::compute(clipped, params);
        let bytes_in = clipped.width as usize * clipped.height as usize * 4;
        let bytes_out = bytes_in;

        let mut backend = BackendKind::Cpu;
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if self.prefer_gpu && !self.gpu_failed {
            let mut probe_error = None;
            if self.gpu.is_none() {
                match gpu::GpuBackend::new() {
                    Ok(created) => self.gpu = Some(created),
                    Err(error) => probe_error = Some(error),
                }
            }
            if let Some(error) = probe_error {
                log_backend_failure(&error);
                self.gpu_failed = true;
            } else if let Some(gpu) = self.gpu.as_mut() {
                if gpu.is_usable() {
                    match gpu.apply(src, &plan, params, out) {
                        Ok(()) => backend = BackendKind::Gpu,
                        Err(error) => {
                            log_backend_failure(&error);
                            gpu.disable();
                            self.gpu_failed = true;
                        }
                    }
                } else {
                    self.gpu_failed = true;
                }
            }
        }

        if backend == BackendKind::Cpu {
            self.cpu.apply(*src, &plan, params, out)?;
        }

        self.stats = FrameStats {
            backend: Some(backend),
            duration: started.elapsed(),
            bytes_read: bytes_in,
            bytes_written: bytes_out,
        };
        self.adaptive.observe(self.stats.duration);
        Ok(self.stats)
    }

    /// Convenience wrapper that allocates (or reuses) an output buffer.
    pub fn apply_into_vec(
        &mut self,
        src: &ImageRef<'_>,
        region: Rect,
        params: &BackdropParams,
        out: &mut Vec<u8>,
    ) -> Result<FrameStats, Error> {
        let needed = region.width as usize * region.height as usize * 4;
        if out.len() != needed {
            out.resize(needed, 0);
        }
        self.apply(src, region, params, out)
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn log_backend_failure(error: &Error) {
    // A failing GPU is not fatal: the next frame falls back to the CPU. Keep
    // the message cheap and dependency-free.
    if std::env::var_os("EFFIBUDDY_BACKDROP_DEBUG").is_some() {
        eprintln!("[backdrop] GPU path disabled: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let index = (y * width as usize + x) * 4;
                data[index] = (x * 255 / width.max(1) as usize) as u8;
                data[index + 1] = (y * 255 / height.max(1) as usize) as u8;
                data[index + 2] = 128;
                data[index + 3] = 255;
            }
        }
        data
    }

    #[test]
    fn blur_preserves_a_flat_field() {
        let source = vec![90u8; 64 * 64 * 4];
        let mut out = vec![0u8; 64 * 64 * 4];
        let mut filter = BackdropFilter::with_gpu(false);
        let params = BackdropParams {
            blur_radius: 16.0,
            ..BackdropParams::default()
        };
        filter
            .apply(&ImageRef::new(&source, 64, 64), Rect::new(0, 0, 64, 64), &params, &mut out)
            .unwrap();
        for pixel in out.chunks_exact(4) {
            assert!(pixel[..3].iter().all(|&c| (c as i32 - 90).abs() <= 1), "{pixel:?}");
        }
    }

    #[test]
    fn blur_reduces_local_variance() {
        let width = 128;
        let height = 128;
        let mut source = gradient(width, height);
        // Add a sharp checkerboard on the red channel.
        for y in 0..height as usize {
            for x in 0..width as usize {
                let index = (y * width as usize + x) * 4;
                if (x / 8 + y / 8) % 2 == 0 {
                    source[index] = 255;
                } else {
                    source[index] = 0;
                }
            }
        }

        let mut blurred = vec![0u8; (width * height * 4) as usize];
        let mut filter = BackdropFilter::with_gpu(false);
        filter
            .apply(
                &ImageRef::new(&source, width, height),
                Rect::new(0, 0, width, height),
                &BackdropParams {
                    blur_radius: 12.0,
                    ..BackdropParams::default()
                },
                &mut blurred,
            )
            .unwrap();

        let variance = |data: &[u8]| {
            let values: Vec<f64> = data.chunks_exact(4).map(|p| p[0] as f64).collect();
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64
        };
        assert!(
            variance(&blurred) < variance(&source) * 0.5,
            "blur did not smooth the checkerboard"
        );
    }

    #[test]
    fn zero_blur_is_a_tint_only_copy() {
        let source = gradient(32, 32);
        let mut out = vec![0u8; 32 * 32 * 4];
        let mut filter = BackdropFilter::with_gpu(false);
        filter
            .apply(
                &ImageRef::new(&source, 32, 32),
                Rect::new(0, 0, 32, 32),
                &BackdropParams {
                    blur_radius: 0.0,
                    ..BackdropParams::default()
                },
                &mut out,
            )
            .unwrap();
        for (a, b) in source.chunks_exact(4).zip(out.chunks_exact(4)) {
            assert!(a[..3].iter().zip(&b[..3]).all(|(x, y)| (*x as i32 - *y as i32).abs() <= 1));
        }
    }

    #[test]
    fn corners_are_masked_out() {
        let source = vec![255u8; 64 * 64 * 4];
        let mut out = vec![0u8; 64 * 64 * 4];
        let mut filter = BackdropFilter::with_gpu(false);
        filter
            .apply(
                &ImageRef::new(&source, 64, 64),
                Rect::new(0, 0, 64, 64),
                &BackdropParams {
                    blur_radius: 8.0,
                    corners: CornerRadius::all(20.0),
                    ..BackdropParams::default()
                },
                &mut out,
            )
            .unwrap();
        assert_eq!(out[3], 0, "top-left corner pixel must be transparent");
        let center = (32 * 64 + 32) * 4;
        assert_eq!(out[center + 3], 255, "center must stay opaque");
    }

    #[test]
    fn region_outside_source_is_rejected() {
        let source = vec![0u8; 16 * 16 * 4];
        let mut out = vec![0u8; 32 * 32 * 4];
        let mut filter = BackdropFilter::with_gpu(false);
        let error = filter
            .apply(
                &ImageRef::new(&source, 16, 16),
                Rect::new(0, 0, 32, 32),
                &BackdropParams::default(),
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(error, Error::EmptyRegion | Error::SourceTooSmall));
    }

    #[test]
    fn sub_region_matches_whole_image_crop() {
        let width = 96;
        let height = 96;
        let source = gradient(width, height);
        let image = ImageRef::new(&source, width, height);
        let region = Rect::new(16, 24, 48, 40);
        let params = BackdropParams {
            blur_radius: 10.0,
            ..BackdropParams::default()
        };

        let mut cropped = Vec::new();
        let mut filter = BackdropFilter::with_gpu(false);
        filter.apply_into_vec(&image, region, &params, &mut cropped).unwrap();

        // Same region, but sourced from an image view that starts at the region.
        let view = image.region(region.x as u32, region.y as u32, region.width, region.height);
        let mut direct = Vec::new();
        filter
            .apply_into_vec(&view, Rect::new(0, 0, region.width, region.height), &params, &mut direct)
            .unwrap();

        let max_diff = cropped
            .iter()
            .zip(&direct)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(max_diff <= 1, "region views must agree, diff {max_diff}");
    }

    #[test]
    fn frame_budget_downgrades_under_pressure() {
        let source = vec![120u8; 64 * 64 * 4];
        let image = ImageRef::new(&source, 64, 64);
        let region = Rect::new(0, 0, 64, 64);
        let mut out = Vec::new();
        let params = BackdropParams {
            quality: Quality::High,
            ..BackdropParams::default()
        };

        let mut filter = BackdropFilter::with_gpu(false);
        // 1ns budget: every frame is over, so the third frame must downgrade.
        filter.set_frame_budget(Some(Duration::from_nanos(1)));
        assert_eq!(filter.effective_quality(Quality::High), Quality::High);
        for _ in 0..AdaptiveQuality::DOWNGRADE_AFTER_FRAMES {
            filter.apply_into_vec(&image, region, &params, &mut out).unwrap();
        }
        assert!(filter.is_downgraded());
        assert_eq!(filter.effective_quality(Quality::High), Quality::Fast);
        // The effect still renders, just with the cheaper plan.
        filter.apply_into_vec(&image, region, &params, &mut out).unwrap();
        // Uniform source: alpha passes through (no corner mask requested).
        assert!(out.chunks_exact(4).all(|pixel| pixel[3] == 120));
    }

    #[test]
    fn frame_budget_keeps_quality_when_comfortable() {
        let source = vec![90u8; 32 * 32 * 4];
        let image = ImageRef::new(&source, 32, 32);
        let region = Rect::new(0, 0, 32, 32);
        let mut out = Vec::new();
        let params = BackdropParams::default();

        let mut filter = BackdropFilter::with_gpu(false);
        filter.set_frame_budget(Some(Duration::from_millis(50)));
        for _ in 0..40 {
            filter.apply_into_vec(&image, region, &params, &mut out).unwrap();
        }
        assert!(!filter.is_downgraded());
        assert_eq!(filter.effective_quality(Quality::High), Quality::High);
    }

    #[test]
    fn disabling_the_budget_drops_the_downgrade() {
        let source = vec![10u8; 16 * 16 * 4];
        let image = ImageRef::new(&source, 16, 16);
        let region = Rect::new(0, 0, 16, 16);
        let mut out = Vec::new();
        let params = BackdropParams::default();

        let mut filter = BackdropFilter::with_gpu(false);
        filter.set_frame_budget(Some(Duration::from_nanos(1)));
        for _ in 0..8 {
            filter.apply_into_vec(&image, region, &params, &mut out).unwrap();
        }
        assert!(filter.is_downgraded());
        filter.set_frame_budget(None);
        assert!(!filter.is_downgraded());
        assert_eq!(filter.effective_quality(Quality::High), Quality::High);
    }
}
