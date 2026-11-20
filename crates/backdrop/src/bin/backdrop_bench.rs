//! Frames-per-second harness for the backdrop pipeline.
//!
//! Run with `cargo run --release -p effibuddy-backdrop --bin backdrop-bench`.
//! See `docs/BACKDROP_FILTER.md` for what the numbers mean.

use std::time::Instant;

use effibuddy_backdrop::{
    BackdropFilter, BackdropParams, CornerRadius, FrameStats, ImageRef, Quality, Rect, Rgba8,
};

/// A synthetic "UI backdrop": a window-sized frame with gradients, text-like
/// high-frequency blocks and a photo-ish patch, so the blurs see detail at
/// every scale instead of a flat field.
fn make_frame(width: u32, height: u32) -> Vec<u8> {
    let mut data = vec![0u8; width as usize * height as usize * 4];
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 33) as u32
    };

    for y in 0..height as usize {
        for x in 0..width as usize {
            let index = (y * width as usize + x) * 4;
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            let mut r = (fx * 180.0 + 30.0) as u32;
            let mut g = (fy * 140.0 + 50.0) as u32;
            let mut b = ((1.0 - fx) * 120.0 + 70.0) as u32;

            // "Text": 2px stripes on a 7px pitch.
            if (y / 7) % 3 == 0 && x % 7 < 2 {
                r = 20;
                g = 22;
                b = 26;
            }
            // "Chrome": a bright band with a hard edge.
            if (x as f32 / width as f32 - 0.62).abs() < 0.01 {
                r = 250;
                g = 250;
                b = 250;
            }
            // Fine noise, like a photograph.
            let grain = next() & 0x1F;
            r = (r + grain).min(255);
            g = (g + (grain >> 1)).min(255);
            b = (b + (grain >> 2)).min(255);

            data[index] = r as u8;
            data[index + 1] = g as u8;
            data[index + 2] = b as u8;
            data[index + 3] = 255;
        }
    }
    data
}

struct Case {
    name: &'static str,
    region: (u32, u32, u32, u32),
    params: BackdropParams,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "menu 320x240 r=18",
            region: (720, 300, 320, 240),
            params: BackdropParams {
                blur_radius: 18.0,
                ..BackdropParams::default()
            },
        },
        Case {
            name: "dialog 800x600 r=24",
            region: (560, 240, 800, 600),
            params: BackdropParams {
                blur_radius: 24.0,
                saturation: 1.2,
                brightness: 1.05,
                tint: Rgba8::from_rgb_alpha(0xFFFFFF, 38),
                corners: CornerRadius::all(14.0),
                ..BackdropParams::default()
            },
        },
        Case {
            name: "sidebar 380x1080 r=32",
            region: (0, 0, 380, 1080),
            params: BackdropParams {
                blur_radius: 32.0,
                ..BackdropParams::default()
            },
        },
        Case {
            name: "full 1920x1080 r=24",
            region: (0, 0, 1920, 1080),
            params: BackdropParams {
                blur_radius: 24.0,
                saturation: 1.15,
                tint: Rgba8::from_rgb_alpha(0xFFFFFF, 28),
                corners: CornerRadius::all(18.0),
                ..BackdropParams::default()
            },
        },
        Case {
            name: "full 1920x1080 r=64",
            region: (0, 0, 1920, 1080),
            params: BackdropParams {
                blur_radius: 64.0,
                quality: Quality::Fast,
                ..BackdropParams::default()
            },
        },
    ]
}

fn measure(
    filter: &mut BackdropFilter,
    src: &ImageRef<'_>,
    region: Rect,
    params: &BackdropParams,
    out: &mut [u8],
    iterations: usize,
) -> (FrameStats, (f64, f64)) {
    // Warm up: page in buffers, JIT the driver, populate caches.
    for _ in 0..3 {
        filter.apply(src, region, params, out).unwrap();
    }
    let mut best = f64::INFINITY;
    let mut total = 0.0;
    let mut last = FrameStats::default();
    for _ in 0..iterations {
        let started = Instant::now();
        last = filter.apply(src, region, params, out).unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        total += elapsed;
        best = best.min(elapsed);
    }
    (last, (best, total / iterations as f64))
}

fn main() {
    let width = 1920u32;
    let height = 1080u32;
    let source = make_frame(width, height);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let iterations: usize = args
        .iter()
        .find_map(|arg| arg.parse().ok())
        .unwrap_or(240);
    let cpu_only = args.iter().any(|arg| arg == "--cpu");
    // `--min-fps 300` turns the run into a gate: exit code 1 if any case misses
    // the budget. Used by the performance lab / CI.
    let min_fps: f64 = args
        .iter()
        .position(|arg| arg == "--min-fps")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.0);

    let mut filter = BackdropFilter::with_gpu(!cpu_only);
    println!(
        "backdrop-bench: {} ({} cores visible)",
        filter.accelerator(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>12}",
        "case", "best ms", "best fps", "mean fps", "GB/s"
    );

    let image = ImageRef::new(&source, width, height);
    let mut out = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for case in cases() {
        let (cx, cy, cw, ch) = case.region;
        let region = Rect::new(cx as i32, cy as i32, cw, ch);
        out.resize((cw * ch * 4) as usize, 0);
        let (stats, timings) = measure(
            &mut filter,
            &image,
            region,
            &case.params,
            &mut out,
            iterations,
        );
        println!(
            "{:<24} {:>10.3} {:>10.0} {:>10.0} {:>12.1}",
            case.name,
            timings.0 * 1000.0,
            1.0 / timings.0,
            1.0 / timings.1,
            stats.bandwidth() / 1e9
        );
        if 1.0 / timings.0 < min_fps {
            failures.push(format!("{} ({:.0} fps)", case.name, 1.0 / timings.0));
        }
    }

    if !cpu_only && filter.backend() == effibuddy_backdrop::BackendKind::Gpu {
        // Same table, CPU only, for comparison on the same machine.
        let mut cpu_filter = BackdropFilter::with_gpu(false);
        println!("\nCPU backend for comparison:");
        println!(
            "{:<24} {:>10} {:>10} {:>10} {:>12}",
            "case", "best ms", "best fps", "mean fps", "GB/s"
        );
        for case in cases() {
            let (cx, cy, cw, ch) = case.region;
            let region = Rect::new(cx as i32, cy as i32, cw, ch);
            out.resize((cw * ch * 4) as usize, 0);
            let (stats, timings) = measure(
                &mut cpu_filter,
                &image,
                region,
                &case.params,
                &mut out,
                iterations,
            );
            println!(
                "{:<24} {:>10.3} {:>10.0} {:>10.0} {:>12.1}",
                case.name,
                timings.0 * 1000.0,
                1.0 / timings.0,
                1.0 / timings.1,
                stats.bandwidth() / 1e9
            );
        }
    }

    if min_fps > 0.0 {
        if failures.is_empty() {
            println!("\nall cases met the {min_fps:.0} fps budget");
        } else {
            eprintln!(
                "\n{} case(s) missed the {min_fps:.0} fps budget: {}",
                failures.len(),
                failures.join(", ")
            );
            std::process::exit(1);
        }
    }
}
