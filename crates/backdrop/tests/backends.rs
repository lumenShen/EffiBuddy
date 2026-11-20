//! Cross-backend agreement: the GPU pipeline must produce the same picture as
//! the CPU pipeline, because both derive their work from `Plan` and share the
//! color stage math.

use effibuddy_backdrop::{
    BackdropFilter, BackdropParams, BackendKind, CornerRadius, ImageRef, Quality, Rect, Rgba8,
};

fn scene(width: u32, height: u32) -> Vec<u8> {
    let mut data = vec![0u8; width as usize * height as usize * 4];
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 40) as u8
    };
    for y in 0..height as usize {
        for x in 0..width as usize {
            let index = (y * width as usize + x) * 4;
            // Half smooth gradient, half noise: exercises the blur's shoulders.
            let smooth = ((x * 255 / width.max(1) as usize) + (y * 255 / height.max(1) as usize)) / 2;
            let value = if (x / 32 + y / 32) % 2 == 0 {
                smooth as u8
            } else {
                next()
            };
            data[index] = value;
            data[index + 1] = value.wrapping_add(40);
            data[index + 2] = 255 - value;
            data[index + 3] = 255;
        }
    }
    data
}

fn compare(name: &str, params: BackdropParams, tolerance: f32, region: Rect) {
    let width = 320;
    let height = 240;
    let source = scene(width, height);
    let image = ImageRef::new(&source, width, height);
    let mut gpu_filter = BackdropFilter::with_gpu(true);
    let mut cpu_filter = BackdropFilter::with_gpu(false);

    let mut gpu_out = Vec::new();
    let mut cpu_out = Vec::new();

    let gpu_stats = gpu_filter
        .apply_into_vec(&image, region, &params, &mut gpu_out)
        .expect("gpu apply");
    cpu_filter
        .apply_into_vec(&image, region, &params, &mut cpu_out)
        .expect("cpu apply");

    if gpu_stats.backend != Some(BackendKind::Gpu) {
        eprintln!("{name}: no GPU backend on this machine, skipping");
        return;
    }

    assert_eq!(gpu_out.len(), cpu_out.len());
    let mut total = 0u64;
    let mut worst = 0i32;
    for (a, b) in gpu_out.iter().zip(&cpu_out) {
        let diff = (*a as i32 - *b as i32).abs();
        total += diff as u64;
        worst = worst.max(diff);
    }
    let mean = total as f32 / gpu_out.len() as f32;
    // The CPU path accumulates in 16.16 fixed point and truncates while the GPU
    // path uses f32 and rounds, so a 1-2 LSB spread is expected and invisible.
    assert!(
        mean <= tolerance && worst <= 6,
        "{name}: GPU and CPU disagree (mean {mean:.3}, worst {worst})"
    );
}

#[test]
fn gpu_matches_cpu_for_typical_glass_panel() {
    compare(
        "dialog",
        BackdropParams {
            blur_radius: 24.0,
            saturation: 1.2,
            brightness: 1.05,
            contrast: 0.95,
            tint: Rgba8::from_rgb_alpha(0xFFFFFF, 32),
            corners: CornerRadius::all(14.0),
            ..BackdropParams::default()
        },
        1.5,
        Rect::new(24, 16, 200, 160),
    );
}

#[test]
fn gpu_matches_cpu_for_wide_blur() {
    compare(
        "wide blur",
        BackdropParams {
            blur_radius: 72.0,
            quality: Quality::High,
            ..BackdropParams::default()
        },
        1.5,
        Rect::new(24, 16, 200, 160),
    );
}

#[test]
fn gpu_matches_cpu_without_blur() {
    compare(
        "no blur",
        BackdropParams {
            blur_radius: 0.0,
            saturation: 0.6,
            tint: Rgba8::from_rgb_alpha(0x101820, 96),
            ..BackdropParams::default()
        },
        1.5,
        Rect::new(24, 16, 200, 160),
    );
}

#[test]
fn gpu_matches_cpu_for_tiny_region() {
    compare(
        "tiny",
        BackdropParams {
            blur_radius: 8.0,
            corners: CornerRadius::all(6.0),
            ..BackdropParams::default()
        },
        1.5,
        Rect::new(100, 120, 37, 23),
    );
}
