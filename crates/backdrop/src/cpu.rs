//! CPU backend: rayon-parallel, cache-friendly, operating on 8-bit RGBA.
//!
//! The pipeline is deliberately bandwidth-shaped rather than compute-shaped:
//!
//! 1. `downsample` — one pass over the source, `n x n` box average into a
//!    buffer that is `n^2` times smaller. This is the only pass that reads the
//!    whole source and it is also the first half of the low-pass.
//! 2. `blur_h` / `blur_v` — separable fixed-point Gaussian at low resolution.
//!    Because (1) already divided sigma by `n`, the kernel stays at ~11 taps.
//! 3. `composite` — bilinear upsample fused with the color stage, the tint and
//!    the rounded-corner mask, writing the result exactly once.
//!
//! Two things keep `composite` close to the memory floor:
//!
//! * the corner mask is rendered once per (size, radius) into a `u8` plane and
//!   reused, instead of evaluating a rounded-rect SDF per pixel per frame;
//! * the color stage collapses into a single 3x3 matrix plus offset computed
//!   once per frame, and runs as a monomorphised inner loop with no branches
//!   (the identity case skips the matrix entirely).

use rayon::prelude::*;

use crate::error::Error;
use crate::image::{ImageRef, corner_coverage};
use crate::params::{BackdropParams, CornerRadius};
use crate::plan::Plan;

/// Reusable scratch buffers so a steady-state frame allocates nothing.
#[derive(Default)]
pub(crate) struct CpuBackend {
    low_a: Vec<u8>,
    low_b: Vec<u8>,
    /// Corner coverage plane, rebuilt only when the geometry changes.
    mask: Vec<u8>,
    mask_key: Option<MaskKey>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct MaskKey {
    width: u32,
    height: u32,
    corners: CornerRadius,
    softness: u32,
}

/// `brightness -> contrast -> saturate -> tint` collapsed into `m * rgb + o`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColorMatrix {
    m: [f32; 9],
    o: [f32; 3],
    pub(crate) identity: bool,
}

const LUMA_R: f32 = 0.213;
const LUMA_G: f32 = 0.715;
const LUMA_B: f32 = 0.072;
const ONE_OVER_255: f32 = 1.0 / 255.0;

impl ColorMatrix {
    pub(crate) fn from_params(params: &BackdropParams) -> Self {
        // Saturation, contrast and brightness are all affine, so the whole CSS
        // filter list folds into one matrix. Tint is affine too and goes into
        // the offset.
        let s = params.saturation;
        let sat = [
            LUMA_R + 0.787 * s,
            LUMA_G - LUMA_G * s,
            LUMA_B - LUMA_B * s,
            LUMA_R - LUMA_R * s,
            LUMA_G + 0.285 * s,
            LUMA_B - LUMA_B * s,
            LUMA_R - LUMA_R * s,
            LUMA_G - LUMA_G * s,
            LUMA_B + 0.928 * s,
        ];

        // brightness: v *= b, contrast: v = (v - 0.5) * k + 0.5
        let scale = params.brightness * params.contrast;
        let offset = 0.5 - 0.5 * params.contrast;

        let mut m = [0.0f32; 9];
        for row in 0..3 {
            for column in 0..3 {
                m[row * 3 + column] = sat[row * 3 + column] * scale;
            }
        }
        let mut o = [0.0f32; 3];
        for row in 0..3 {
            o[row] = (sat[row * 3] + sat[row * 3 + 1] + sat[row * 3 + 2]) * offset;
        }

        let tint = params.tint;
        if !tint.is_transparent() {
            let a = tint.a as f32 / 255.0;
            let tint_rgb = [
                tint.r as f32 / 255.0,
                tint.g as f32 / 255.0,
                tint.b as f32 / 255.0,
            ];
            for row in 0..3 {
                for column in 0..3 {
                    m[row * 3 + column] *= 1.0 - a;
                }
                o[row] = o[row] * (1.0 - a) + tint_rgb[row] * a;
            }
        }

        let identity = params.is_color_identity();
        Self { m, o, identity }
    }

    #[inline(always)]
    pub(crate) fn apply(&self, rgb: &mut [f32; 3]) {
        let r = rgb[0];
        let g = rgb[1];
        let b = rgb[2];
        rgb[0] = (self.m[0] * r + self.m[1] * g + self.m[2] * b + self.o[0]).clamp(0.0, 1.0);
        rgb[1] = (self.m[3] * r + self.m[4] * g + self.m[5] * b + self.o[1]).clamp(0.0, 1.0);
        rgb[2] = (self.m[6] * r + self.m[7] * g + self.m[8] * b + self.o[2]).clamp(0.0, 1.0);
    }
}

impl CpuBackend {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn apply(
        &mut self,
        src: ImageRef<'_>,
        plan: &Plan,
        params: &BackdropParams,
        out: &mut [u8],
    ) -> Result<(), Error> {
        let region = plan.region;
        if region.is_empty() {
            return Ok(());
        }
        let region_bytes = region.width as usize * region.height as usize * 4;
        if out.len() < region_bytes {
            return Err(Error::OutputTooSmall {
                needed: region_bytes,
                got: out.len(),
            });
        }
        if src.width < region.right().max(0) as u32 || src.height < region.bottom().max(0) as u32 {
            return Err(Error::SourceTooSmall);
        }

        let low_len = plan.low_width as usize * plan.low_height as usize * 4;
        if self.low_a.len() < low_len {
            self.low_a.resize(low_len, 0);
            self.low_b.resize(low_len, 0);
        }

        let debug = std::env::var_os("EFFIBUDDY_BACKDROP_DEBUG").is_some();
        let started = std::time::Instant::now();

        self.downsample(src, plan);
        let after_downsample = started.elapsed();
        if plan.has_blur() {
            self.blur_horizontal(plan);
            self.blur_vertical(plan);
        }
        let after_blur = started.elapsed();
        self.composite(src, plan, params, out);

        if debug {
            let total = started.elapsed();
            eprintln!(
                "[backdrop-cpu] downsample {:.3}ms blur {:.3}ms composite {:.3}ms total {:.3}ms",
                after_downsample.as_secs_f64() * 1e3,
                (after_blur - after_downsample).as_secs_f64() * 1e3,
                (total - after_blur).as_secs_f64() * 1e3,
                total.as_secs_f64() * 1e3
            );
        }
        Ok(())
    }

    fn downsample(&mut self, src: ImageRef<'_>, plan: &Plan) {
        let region = plan.region;
        let n = plan.downsample as usize;
        let low_width = plan.low_width as usize;
        let low_height = plan.low_height as usize;
        let region_width = region.width as usize;
        let region_height = region.height as usize;
        let stride = src.stride;
        let base_y = region.y as usize;
        let base_x = region.x as usize;
        let source = &src.data[base_y * stride..];
        let row_bytes = low_width * 4;
        let shift = n.trailing_zeros() * 2;
        let power_of_two = n.is_power_of_two();
        let inv = 1.0 / (n * n) as f32;

        self.low_a[..low_height * row_bytes]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(sy, out_row)| {
                let y0 = sy * n;
                for sx in 0..low_width {
                    let x0 = sx * n;
                    let mut acc = [0u32; 4];
                    for dy in 0..n {
                        let y = (y0 + dy).min(region_height - 1);
                        let row = &source[y * stride + base_x * 4..];
                        for dx in 0..n {
                            let x = (x0 + dx).min(region_width - 1);
                            let pixel = u32::from_le_bytes([
                                row[x * 4],
                                row[x * 4 + 1],
                                row[x * 4 + 2],
                                row[x * 4 + 3],
                            ]);
                            acc[0] += pixel & 0xFF;
                            acc[1] += (pixel >> 8) & 0xFF;
                            acc[2] += (pixel >> 16) & 0xFF;
                            acc[3] += pixel >> 24;
                        }
                    }
                    let dst = &mut out_row[sx * 4..sx * 4 + 4];
                    if power_of_two {
                        dst[0] = (acc[0] >> shift) as u8;
                        dst[1] = (acc[1] >> shift) as u8;
                        dst[2] = (acc[2] >> shift) as u8;
                        dst[3] = (acc[3] >> shift) as u8;
                    } else {
                        for channel in 0..4 {
                            dst[channel] = (acc[channel] as f32 * inv).round() as u8;
                        }
                    }
                }
            });
    }

    fn blur_horizontal(&mut self, plan: &Plan) {
        let low_len = plan.low_width as usize * plan.low_height as usize * 4;
        let CpuBackend { low_a, low_b, .. } = self;
        convolve(low_a, low_b.as_mut_slice(), low_len, Axis::Horizontal, plan);
    }

    fn blur_vertical(&mut self, plan: &Plan) {
        let low_len = plan.low_width as usize * plan.low_height as usize * 4;
        let CpuBackend { low_a, low_b, .. } = self;
        convolve(low_b, low_a.as_mut_slice(), low_len, Axis::Vertical, plan);
    }

    fn composite(
        &mut self,
        src: ImageRef<'_>,
        plan: &Plan,
        params: &BackdropParams,
        out: &mut [u8],
    ) {
        let color = ColorMatrix::from_params(params);
        let identity = color.identity;
        let CpuBackend {
            low_a,
            mask,
            mask_key,
            ..
        } = self;
        match (ensure_mask(mask, mask_key, plan, params), identity) {
            (None, true) => composite_impl::<false, true>(low_a, src, plan, &color, None, out),
            (None, false) => composite_impl::<false, false>(low_a, src, plan, &color, None, out),
            (Some(mask), true) => {
                composite_impl::<true, true>(low_a, src, plan, &color, Some(mask), out)
            }
            (Some(mask), false) => {
                composite_impl::<true, false>(low_a, src, plan, &color, Some(mask), out)
            }
        }
    }
}

/// Rebuild the corner coverage plane if the geometry changed, and hand back a
/// view of it. Split out of `CpuBackend` so the composite pass can borrow the
/// scratch buffers and the plane at the same time.
fn ensure_mask<'a>(
    mask: &'a mut Vec<u8>,
    key_slot: &mut Option<MaskKey>,
    plan: &Plan,
    params: &BackdropParams,
) -> Option<&'a [u8]> {
    if params.corners.is_zero() {
        return None;
    }
    let key = MaskKey {
        width: plan.region.width,
        height: plan.region.height,
        corners: params.corners,
        softness: params.edge_softness.max(0.01).to_bits(),
    };
    if *key_slot != Some(key) {
        let width = key.width as usize;
        let height = key.height as usize;
        mask.resize(width * height, 0);
        mask.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            let py = y as f32 + 0.5;
            for (x, slot) in row.iter_mut().enumerate() {
                let coverage = corner_coverage(
                    x as f32 + 0.5,
                    py,
                    key.width as f32,
                    key.height as f32,
                    &key.corners,
                    f32::from_bits(key.softness),
                );
                *slot = (coverage * 255.0 + 0.5) as u8;
            }
        });
        *key_slot = Some(key);
    }
    Some(&mask[..])
}

/// The fused upsample + color + mask + store pass.
///
/// Monomorphised on `MASK` and `IDENTITY` so the inner loop has no branches and
/// LLVM can vectorise the channel math. The per-column upsample coordinates are
/// computed once per frame instead of once per pixel, and the four taps are
/// copied into arrays so the channel math runs without bounds checks.
fn composite_impl<const MASK: bool, const IDENTITY: bool>(
    low: &[u8],
    src: ImageRef<'_>,
    plan: &Plan,
    color: &ColorMatrix,
    mask: Option<&[u8]>,
    out: &mut [u8],
) {
    let region = plan.region;
    let width = region.width as usize;
    let height = region.height as usize;
    let low_width = plan.low_width as usize;
    let scale = 1.0 / plan.downsample as f32;
    let low = &low[..low_width * plan.low_height as usize * 4];
    let src_stride = src.stride;
    let source = &src.data[region.y as usize * src_stride + region.x as usize * 4..];
    let max_lx = low_width as isize - 1;
    let max_ly = plan.low_height as isize - 1;
    let row_bytes = width * 4;

    // Column plan: which two low-res columns and how much of each.
    let mut column_x0 = vec![0usize; width];
    let mut column_x1 = vec![0usize; width];
    let mut column_t = vec![0.0f32; width];
    for x in 0..width {
        let fx = (x as f32 + 0.5) * scale - 0.5;
        let x0 = fx.floor();
        column_x0[x] = (x0 as isize).clamp(0, max_lx) as usize * 4;
        column_x1[x] = (x0 as isize + 1).clamp(0, max_lx) as usize * 4;
        column_t[x] = fx - x0;
    }

    out[..height * row_bytes]
        .par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(y, out_row)| {
            let fy = (y as f32 + 0.5) * scale - 0.5;
            let y0 = fy.floor();
            let ty = fy - y0;
            let ly0 = (y0 as isize).clamp(0, max_ly) as usize;
            let ly1 = (y0 as isize + 1).clamp(0, max_ly) as usize;
            let row0 = &low[ly0 * low_width * 4..];
            let row1 = &low[ly1 * low_width * 4..];
            let src_row = &source[y * src_stride..];
            let mask_row = mask.map(|mask| &mask[y * width..]);
            let one_minus_ty = 1.0 - ty;

            for x in 0..width {
                let lx0 = column_x0[x];
                let lx1 = column_x1[x];
                let tx = column_t[x];
                let one_minus_tx = 1.0 - tx;

                let p00: [u8; 4] = row0[lx0..lx0 + 4].try_into().unwrap();
                let p10: [u8; 4] = row0[lx1..lx1 + 4].try_into().unwrap();
                let p01: [u8; 4] = row1[lx0..lx0 + 4].try_into().unwrap();
                let p11: [u8; 4] = row1[lx1..lx1 + 4].try_into().unwrap();

                let w00 = one_minus_tx * one_minus_ty;
                let w10 = tx * one_minus_ty;
                let w01 = one_minus_tx * ty;
                let w11 = tx * ty;

                let mut rgb = [
                    (p00[0] as f32 * w00
                        + p10[0] as f32 * w10
                        + p01[0] as f32 * w01
                        + p11[0] as f32 * w11)
                        * ONE_OVER_255,
                    (p00[1] as f32 * w00
                        + p10[1] as f32 * w10
                        + p01[1] as f32 * w01
                        + p11[1] as f32 * w11)
                        * ONE_OVER_255,
                    (p00[2] as f32 * w00
                        + p10[2] as f32 * w10
                        + p01[2] as f32 * w01
                        + p11[2] as f32 * w11)
                        * ONE_OVER_255,
                ];

                if !IDENTITY {
                    color.apply(&mut rgb);
                }

                let mut alpha = src_row[x * 4 + 3];
                if MASK {
                    if let Some(mask_row) = mask_row {
                        alpha = ((alpha as u16 * mask_row[x] as u16) / 255) as u8;
                    }
                }

                // One four-byte store instead of four bounds-checked ones.
                let pixel = [
                    (rgb[0] * 255.0 + 0.5) as u8,
                    (rgb[1] * 255.0 + 0.5) as u8,
                    (rgb[2] * 255.0 + 0.5) as u8,
                    alpha,
                ];
                out_row[x * 4..x * 4 + 4].copy_from_slice(&pixel);
            }
        });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

fn convolve(src: &[u8], dst: &mut [u8], len: usize, axis: Axis, plan: &Plan) {
    let width = plan.low_width as usize;
    let radius = plan.radius as i32;
    let kernel = &plan.kernel;
    let row_bytes = width * 4;
    let max_x = width as i32 - 1;
    let max_y = plan.low_height as i32 - 1;
    let src = &src[..len];
    let dst = &mut dst[..len];

    dst.par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(y, out_row)| {
            let y = y as i32;
            for x in 0..width {
                let mut acc = [0i32; 4];
                for (t, &weight) in kernel.iter().enumerate() {
                    let offset = match axis {
                        Axis::Horizontal => {
                            let sx = (x as i32 + t as i32 - radius).clamp(0, max_x) as usize;
                            (y as usize * width + sx) * 4
                        }
                        Axis::Vertical => {
                            let sy = (y + t as i32 - radius).clamp(0, max_y) as usize;
                            (sy * width + x) * 4
                        }
                    };
                    let px = &src[offset..offset + 4];
                    acc[0] += px[0] as i32 * weight;
                    acc[1] += px[1] as i32 * weight;
                    acc[2] += px[2] as i32 * weight;
                    acc[3] += px[3] as i32 * weight;
                }
                let dst = &mut out_row[x * 4..x * 4 + 4];
                for channel in 0..4 {
                    dst[channel] = ((acc[channel] >> 16).clamp(0, 255)) as u8;
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{CornerRadius, Rgba8};

    /// Straightforward, un-optimised reference for the documented order:
    /// brightness -> contrast -> saturate -> tint.
    fn reference(rgb: [f32; 3], params: &BackdropParams) -> [f32; 3] {
        let mut out = rgb;
        for channel in out.iter_mut() {
            *channel *= params.brightness;
        }
        for channel in out.iter_mut() {
            *channel = (*channel - 0.5) * params.contrast + 0.5;
        }
        let s = params.saturation;
        let r = out[0];
        let g = out[1];
        let b = out[2];
        out[0] = (LUMA_R + 0.787 * s) * r + (LUMA_G - LUMA_G * s) * g + (LUMA_B - LUMA_B * s) * b;
        out[1] = (LUMA_R - LUMA_R * s) * r + (LUMA_G + 0.285 * s) * g + (LUMA_B - LUMA_B * s) * b;
        out[2] = (LUMA_R - LUMA_R * s) * r + (LUMA_G - LUMA_G * s) * g + (LUMA_B + 0.928 * s) * b;
        let tint = params.tint;
        if !tint.is_transparent() {
            let a = tint.a as f32 / 255.0;
            out[0] = out[0] * (1.0 - a) + tint.r as f32 / 255.0 * a;
            out[1] = out[1] * (1.0 - a) + tint.g as f32 / 255.0 * a;
            out[2] = out[2] * (1.0 - a) + tint.b as f32 / 255.0 * a;
        }
        out
    }

    #[test]
    fn matrix_matches_the_reference_color_stage() {
        let cases = [
            BackdropParams::default(),
            BackdropParams {
                saturation: 1.35,
                brightness: 1.1,
                contrast: 0.9,
                ..BackdropParams::default()
            },
            BackdropParams {
                saturation: 0.0,
                tint: Rgba8::from_rgb_alpha(0x203040, 96),
                ..BackdropParams::default()
            },
            BackdropParams {
                brightness: 0.8,
                tint: Rgba8::opaque(255, 255, 255),
                ..BackdropParams::default()
            },
        ];
        for params in cases {
            let matrix = ColorMatrix::from_params(&params);
            for rgb in [[0.0, 0.0, 0.0], [0.2, 0.6, 0.9], [1.0, 1.0, 1.0], [0.05, 0.5, 0.95]] {
                let mut folded = rgb;
                matrix.apply(&mut folded);
                let expected = reference(rgb, &params);
                for channel in 0..3 {
                    let want = expected[channel].clamp(0.0, 1.0);
                    assert!(
                        (folded[channel] - want).abs() < 1e-5,
                        "{params:?} {rgb:?} channel {channel}: {} vs {}",
                        folded[channel],
                        want
                    );
                }
            }
        }
    }

    #[test]
    fn identity_params_produce_an_identity_matrix() {
        let matrix = ColorMatrix::from_params(&BackdropParams::default());
        assert!(matrix.identity);
        let mut rgb = [0.25, 0.5, 0.75];
        matrix.apply(&mut rgb);
        assert_eq!(rgb, [0.25, 0.5, 0.75]);
    }

    #[test]
    fn zero_saturation_is_luminance() {
        let params = BackdropParams {
            saturation: 0.0,
            ..BackdropParams::default()
        };
        let mut rgb = [0.2, 0.6, 0.9];
        ColorMatrix::from_params(&params).apply(&mut rgb);
        let luma = LUMA_R * 0.2 + LUMA_G * 0.6 + LUMA_B * 0.9;
        for channel in rgb {
            assert!((channel - luma).abs() < 1e-5, "{channel} vs {luma}");
        }
    }

    #[test]
    fn mask_is_cached_between_frames() {
        let mut mask = Vec::new();
        let mut key = None;
        let params = BackdropParams {
            corners: CornerRadius::all(12.0),
            ..BackdropParams::default()
        };
        let plan = Plan::compute(crate::plan::Rect::new(0, 0, 64, 48), &params);
        let first = ensure_mask(&mut mask, &mut key, &plan, &params).unwrap().to_vec();
        let second = ensure_mask(&mut mask, &mut key, &plan, &params).unwrap();
        assert_eq!(first, second, "the same geometry must reuse the cached plane");
        let before = key;
        let other = BackdropParams {
            corners: CornerRadius::all(4.0),
            ..BackdropParams::default()
        };
        let reprojected = ensure_mask(&mut mask, &mut key, &plan, &other)
            .unwrap()
            .to_vec();
        assert_ne!(first, reprojected, "a new radius must rebuild the plane");
        assert_ne!(before, key, "the cache key must change");
    }
}
