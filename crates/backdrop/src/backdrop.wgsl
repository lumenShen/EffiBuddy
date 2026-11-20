// Backdrop-filter compute passes.
//
// Blade injects the @group/@binding attributes from the `ShaderData` layout, so
// the declaration order here must match `PassData` in gpu.rs:
//   params   (uniform)
//   b_source (storage) — the source region, tightly packed RGBA8
//   b_input  (storage)
//   b_output (storage)
//
// Buffers hold one `u32` per pixel (RGBA8 little endian) so every pass touches
// four bytes per pixel and no pass has to deal with channel-planar layouts.

struct Params {
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,

    src_stride_px: u32,
    low_w: u32,
    low_h: u32,
    downsample: u32,

    taps: u32,
    radius: u32,
    pad0: u32,
    pad1: u32,

    // Seven vec4s carry up to 28 kernel weights; `taps` selects how many are
    // live. vec4 packing keeps the uniform layout stable across drivers.
    kernel: array<vec4<f32>, 7>,

    brightness: f32,
    contrast: f32,
    saturation: f32,
    softness: f32,

    tint: vec4<f32>,
    corners: vec4<f32>,
    size: vec2<f32>,
    pad2: vec2<f32>,
};

var<uniform> params: Params;
var<storage, read_write> b_source: array<u32>;
var<storage, read_write> b_input: array<u32>;
var<storage, read_write> b_output: array<u32>;

const KERNEL_SLOTS: u32 = 28u;

fn weight(index: u32) -> f32 {
    let safe = min(index, KERNEL_SLOTS - 1u);
    return params.kernel[safe / 4u][safe % 4u];
}

fn unpack(p: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(p & 0xFFu),
        f32((p >> 8u) & 0xFFu),
        f32((p >> 16u) & 0xFFu),
        f32((p >> 24u) & 0xFFu),
    ) * (1.0 / 255.0);
}

fn pack(color: vec4<f32>) -> u32 {
    let scaled = round(clamp(color, vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0);
    let bytes = vec4<u32>(scaled);
    return bytes.x | (bytes.y << 8u) | (bytes.z << 16u) | (bytes.w << 24u);
}

// The source region is tightly packed, so a source pixel is just (x, y).
fn source_pixel(x: i32, y: i32) -> vec4<f32> {
    let cx = clamp(x, 0, i32(params.region_w) - 1);
    let cy = clamp(y, 0, i32(params.region_h) - 1);
    return unpack(b_source[u32(cy) * params.region_w + u32(cx)]);
}

fn low_index(x: u32, y: u32) -> u32 {
    return y * params.low_w + x;
}

@compute @workgroup_size(16, 16, 1)
fn downsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.low_w || gid.y >= params.low_h) {
        return;
    }
    let n = i32(params.downsample);
    let base_x = i32(gid.x) * n;
    let base_y = i32(gid.y) * n;
    var sum = vec4<f32>(0.0);
    for (var dy = 0; dy < n; dy = dy + 1) {
        for (var dx = 0; dx < n; dx = dx + 1) {
            sum = sum + source_pixel(base_x + dx, base_y + dy);
        }
    }
    b_output[low_index(gid.x, gid.y)] = pack(sum * (1.0 / f32(n * n)));
}

@compute @workgroup_size(16, 16, 1)
fn blur_h(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.low_w || gid.y >= params.low_h) {
        return;
    }
    let max_x = i32(params.low_w) - 1;
    var acc = vec4<f32>(0.0);
    for (var t = 0u; t < params.taps; t = t + 1u) {
        let sx = clamp(i32(gid.x) + i32(t) - i32(params.radius), 0, max_x);
        acc = acc + unpack(b_input[low_index(u32(sx), gid.y)]) * weight(t);
    }
    b_output[low_index(gid.x, gid.y)] = pack(acc);
}

@compute @workgroup_size(16, 16, 1)
fn blur_v(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.low_w || gid.y >= params.low_h) {
        return;
    }
    let max_y = i32(params.low_h) - 1;
    var acc = vec4<f32>(0.0);
    for (var t = 0u; t < params.taps; t = t + 1u) {
        let sy = clamp(i32(gid.y) + i32(t) - i32(params.radius), 0, max_y);
        acc = acc + unpack(b_input[low_index(gid.x, u32(sy))]) * weight(t);
    }
    b_output[low_index(gid.x, gid.y)] = pack(acc);
}

// Signed distance to the rounded rectangle, negative inside. Mirrors
// `image::rounded_rect_sdf` so both backends agree on the mask.
fn rounded_rect_distance(p: vec2<f32>) -> f32 {
    let half = params.size * 0.5;
    let q = p - half;
    var radius = params.corners.z;
    if (q.x < 0.0) {
        if (q.y < 0.0) {
            radius = params.corners.x;
        } else {
            radius = params.corners.w;
        }
    } else if (q.y < 0.0) {
        radius = params.corners.y;
    }
    let r = clamp(radius, 0.0, min(half.x, half.y));
    let inner = vec2<f32>(half.x - r, half.y - r);
    let d = abs(q) - inner;
    let outside = length(max(d, vec2<f32>(0.0)));
    return outside + min(max(d.x, d.y), 0.0) - r;
}

fn coverage(p: vec2<f32>) -> f32 {
    if (params.corners.x + params.corners.y + params.corners.z + params.corners.w <= 0.0) {
        return 1.0;
    }
    let softness = max(params.softness, 0.01);
    let d = rounded_rect_distance(p);
    return clamp((softness * 0.5 - d) / softness, 0.0, 1.0);
}

@compute @workgroup_size(16, 16, 1)
fn composite(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.region_w || gid.y >= params.region_h) {
        return;
    }

    // Bilinear upsample from the low-resolution blur.
    let scale = 1.0 / f32(params.downsample);
    let fx = (f32(gid.x) + 0.5) * scale - 0.5;
    let fy = (f32(gid.y) + 0.5) * scale - 0.5;
    let base = floor(vec2<f32>(fx, fy));
    let t = vec2<f32>(fx, fy) - base;
    let max_lx = i32(params.low_w) - 1;
    let max_ly = i32(params.low_h) - 1;
    let x0 = u32(clamp(i32(base.x), 0, max_lx));
    let x1 = u32(clamp(i32(base.x) + 1, 0, max_lx));
    let y0 = u32(clamp(i32(base.y), 0, max_ly));
    let y1 = u32(clamp(i32(base.y) + 1, 0, max_ly));

    let p00 = unpack(b_input[low_index(x0, y0)]);
    let p10 = unpack(b_input[low_index(x1, y0)]);
    let p01 = unpack(b_input[low_index(x0, y1)]);
    let p11 = unpack(b_input[low_index(x1, y1)]);
    let top = mix(p00, p10, t.x);
    let bottom = mix(p01, p11, t.x);
    var rgb = mix(top, bottom, t.y).rgb;

    // brightness -> contrast -> saturate, matching `ColorMatrix` in cpu.rs.
    rgb = rgb * params.brightness;
    rgb = (rgb - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5);

    let s = params.saturation;
    let gray = vec3<f32>(0.213, 0.715, 0.072);
    let saturated = vec3<f32>(
        (gray.x + 0.787 * s) * rgb.r + (gray.y - gray.y * s) * rgb.g + (gray.z - gray.z * s) * rgb.b,
        (gray.x - gray.x * s) * rgb.r + (gray.y + 0.285 * s) * rgb.g + (gray.z - gray.z * s) * rgb.b,
        (gray.x - gray.x * s) * rgb.r + (gray.y - gray.y * s) * rgb.g + (gray.z + 0.928 * s) * rgb.b,
    );
    // Tint is folded into the same affine stage as the color matrix on the CPU
    // side, so both backends clamp once, at pack time.
    rgb = mix(saturated, params.tint.rgb, params.tint.a);

    let source_alpha = unpack(b_source[gid.y * params.region_w + gid.x]).a;
    let mask = coverage(vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5));
    b_output[gid.y * params.region_w + gid.x] = pack(vec4<f32>(rgb, source_alpha * mask));
}
