//! Vulkan compute backend.
//!
//! Runs the same four passes as the CPU backend (`downsample`, `blur_h`,
//! `blur_v`, `composite`) as compute dispatches through `blade-graphics`, the
//! same abstraction GPUI renders the window with.
//!
//! Buffers are `Memory::Shared` for the two host-facing ends (source upload and
//! result download) and `Memory::Device` for the low-resolution working set, so
//! the wide working set never leaves VRAM.
//!
//! Profiling notes (Ryzen 9 8945HX + RTX 5060 Max-Q, 1920x1080 region):
//! * the dispatches themselves are ~60-120us,
//! * the 8 MB upload and 8 MB readback dominate and are the reason
//!   [`GpuBackend`] reports its own bandwidth to the caller.

use blade_graphics as gpu;
use bytemuck::{Pod, Zeroable};

use crate::error::Error;
use crate::image::ImageRef;
use crate::params::BackdropParams;
use crate::plan::Plan;

/// WGSL for all four passes; kept next to the code so the two stay in sync.
const SHADER_SOURCE: &str = include_str!("backdrop.wgsl");

/// Matches `struct Params` in `backdrop.wgsl` byte for byte.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
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

    /// Up to 28 weights, zero padded.
    kernel: [f32; 28],

    brightness: f32,
    contrast: f32,
    saturation: f32,
    softness: f32,

    tint: [f32; 4],
    corners: [f32; 4],
    size: [f32; 2],
    pad2: [f32; 2],
}

impl Params {
    fn new(plan: &Plan, params: &BackdropParams) -> Self {
        let region = plan.region;
        let mut kernel = [0.0f32; 28];
        let taps = plan.kernel.len().min(28);
        for (slot, weight) in kernel.iter_mut().zip(plan.kernel.iter()) {
            // Fixed point -> float, so GPU and CPU see identical weights.
            *slot = *weight as f32 / crate::plan::KERNEL_ONE as f32;
        }

        let tint = params.tint;
        let corners = params.corners;
        Self {
            region_x: region.x.max(0) as u32,
            region_y: region.y.max(0) as u32,
            region_w: region.width,
            region_h: region.height,
            src_stride_px: region.width,
            low_w: plan.low_width,
            low_h: plan.low_height,
            downsample: plan.downsample,
            taps: taps as u32,
            radius: plan.radius,
            pad0: 0,
            pad1: 0,
            kernel,
            brightness: params.brightness,
            contrast: params.contrast,
            saturation: params.saturation,
            softness: params.edge_softness.max(0.01),
            tint: [
                tint.r as f32 / 255.0,
                tint.g as f32 / 255.0,
                tint.b as f32 / 255.0,
                tint.a as f32 / 255.0,
            ],
            corners: [
                corners.top_left,
                corners.top_right,
                corners.bottom_right,
                corners.bottom_left,
            ],
            size: [region.width as f32, region.height as f32],
            pad2: [0.0, 0.0],
        }
    }
}

/// Binding layout shared by every pass: params + three storage buffers.
#[derive(blade_macros::ShaderData)]
struct PassData {
    params: Params,
    b_source: gpu::BufferPiece,
    b_input: gpu::BufferPiece,
    b_output: gpu::BufferPiece,
}

struct Pipelines {
    downsample: gpu::ComputePipeline,
    blur_h: gpu::ComputePipeline,
    blur_v: gpu::ComputePipeline,
    composite: gpu::ComputePipeline,
}

impl Pipelines {
    fn new(ctx: &gpu::Context) -> Self {
        let shader = ctx.create_shader(gpu::ShaderDesc {
            source: SHADER_SOURCE,
        });
        let layout = <PassData as gpu::ShaderData>::layout();
        let layouts = [&layout];
        let build = |name: &'static str, entry: &'static str| {
            ctx.create_compute_pipeline(gpu::ComputePipelineDesc {
                name,
                data_layouts: &layouts,
                compute: shader.at(entry),
            })
        };
        Self {
            downsample: build("backdrop.downsample", "downsample"),
            blur_h: build("backdrop.blur_h", "blur_h"),
            blur_v: build("backdrop.blur_v", "blur_v"),
            composite: build("backdrop.composite", "composite"),
        }
    }
}

/// A buffer that grows on demand and remembers its capacity.
struct GrowableBuffer {
    inner: Option<gpu::Buffer>,
    capacity: u64,
    name: &'static str,
    memory: gpu::Memory,
}

impl GrowableBuffer {
    fn new(name: &'static str, memory: gpu::Memory) -> Self {
        Self {
            inner: None,
            capacity: 0,
            name,
            memory,
        }
    }

    fn ensure(&mut self, ctx: &gpu::Context, bytes: u64) -> gpu::BufferPiece {
        let needed = bytes.max(64);
        if self.capacity < needed || self.inner.is_none() {
            if let Some(old) = self.inner.take() {
                ctx.destroy_buffer(old);
            }
            // Round up so a slightly larger frame does not reallocate again.
            let size = (needed.next_power_of_two()).max(4096);
            let buffer = ctx.create_buffer(gpu::BufferDesc {
                name: self.name,
                size,
                memory: self.memory,
            });
            self.inner = Some(buffer);
            self.capacity = size;
        }
        self.inner.expect("buffer was just created").into()
    }

    /// Host pointer for the mapped range (host-visible memory only).
    fn host_ptr(&self) -> *mut u8 {
        self.inner.expect("buffer was just created").data()
    }

    fn destroy(&mut self, ctx: &gpu::Context) {
        if let Some(buffer) = self.inner.take() {
            ctx.destroy_buffer(buffer);
        }
        self.capacity = 0;
    }
}

/// Timeout for unsynchronised readbacks; a stuck GPU should not hang a frame.
const WAIT_TIMEOUT_MS: u32 = 5_000;
const WORKGROUP: u32 = 16;

pub struct GpuBackend {
    ctx: gpu::Context,
    encoder: gpu::CommandEncoder,
    pipelines: Pipelines,
    source: GrowableBuffer,
    low_a: GrowableBuffer,
    low_b: GrowableBuffer,
    /// Composite target, device-local: no pass ever reads it back on the CPU.
    output: GrowableBuffer,
    /// Host-visible, host-cached copy target for the result (`Memory::Download`
    /// comes from the vendored blade-graphics patch). Staging through it keeps
    /// the readback a cached memcpy: the same memcpy out of `Memory::Shared`
    /// (write-combined BAR) measured 0.04 GB/s on this machine, and out of
    /// `Memory::Upload` (non-cached system memory) 0.3 GB/s.
    staging: GrowableBuffer,
    usable: bool,
    reason: String,
}

impl GpuBackend {
    pub fn new() -> Result<Self, Error> {
        if std::env::var_os("EFFIBUDDY_BACKDROP_NO_GPU").is_some() {
            return Err(Error::GpuUnavailable("disabled by environment".into()));
        }

        // SAFETY: `Context::init` only requires that we do not call it
        // concurrently with itself, and that the returned context outlives every
        // resource created from it. Both hold: the backend owns the context and
        // creates/destroys resources through its own `&self`.
        let ctx = unsafe {
            gpu::Context::init(gpu::ContextDesc {
                presentation: false,
                validation: false,
                ..Default::default()
            })
        }
        .map_err(|error| Error::GpuUnavailable(format!("{error:?}")))?;

        let pipelines = Pipelines::new(&ctx);
        let encoder = ctx.create_command_encoder(gpu::CommandEncoderDesc {
            name: "backdrop",
            buffer_count: 2,
        });

        Ok(Self {
            ctx,
            encoder,
            pipelines,
            source: GrowableBuffer::new("backdrop.source", gpu::Memory::Shared),
            low_a: GrowableBuffer::new("backdrop.low_a", gpu::Memory::Device),
            low_b: GrowableBuffer::new("backdrop.low_b", gpu::Memory::Device),
            output: GrowableBuffer::new("backdrop.output", gpu::Memory::Device),
            staging: GrowableBuffer::new("backdrop.staging", gpu::Memory::Download),
            usable: true,
            reason: String::new(),
        })
    }

    pub fn is_usable(&self) -> bool {
        self.usable
    }

    pub fn disable(&mut self) {
        self.usable = false;
        self.reason = "device error, using CPU".to_owned();
    }

    pub fn describe(&self) -> String {
        if self.usable {
            "vulkan compute (blade)".to_owned()
        } else {
            format!("vulkan unavailable ({})", self.reason)
        }
    }

    pub fn apply(
        &mut self,
        src: &ImageRef<'_>,
        plan: &Plan,
        params: &BackdropParams,
        out: &mut [u8],
    ) -> Result<(), Error> {
        if !self.usable {
            return Err(Error::GpuUnavailable(self.reason.clone()));
        }

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
        let low_bytes = plan.low_width as usize * plan.low_height as usize * 4;

        let source = self.source.ensure(&self.ctx, region_bytes as u64);
        let low_a = self.low_a.ensure(&self.ctx, low_bytes as u64);
        let low_b = self.low_b.ensure(&self.ctx, low_bytes as u64);
        let output = self.output.ensure(&self.ctx, region_bytes as u64);
        let staging = self.staging.ensure(&self.ctx, region_bytes as u64);

        let debug = std::env::var_os("EFFIBUDDY_BACKDROP_DEBUG").is_some();
        let t0 = std::time::Instant::now();

        // Upload the region row by row: the source may be padded.
        unsafe {
            let dst = self.source.host_ptr();
            debug_assert!(!dst.is_null(), "shared memory must be mapped");
            let row_bytes = region.width as usize * 4;
            for row in 0..region.height as usize {
                let src_offset = (region.y as usize + row) * src.stride + region.x as usize * 4;
                std::ptr::copy_nonoverlapping(
                    src.data.as_ptr().add(src_offset),
                    dst.add(row * row_bytes),
                    row_bytes,
                );
            }
        }

        let t_upload = t0.elapsed();
        let values = Params::new(plan, params);
        let groups_low = [
            plan.low_width.div_ceil(WORKGROUP).max(1),
            plan.low_height.div_ceil(WORKGROUP).max(1),
            1,
        ];
        let groups_full = [
            region.width.div_ceil(WORKGROUP).max(1),
            region.height.div_ceil(WORKGROUP).max(1),
            1,
        ];

        let source_piece = source;
        // One compute pass per dispatch: blade emits a memory barrier when a
        // pass ends, and each pass hands its buffer to the next one.
        self.encoder.start();
        {
            let mut pass = self.encoder.compute("backdrop.downsample");
            let mut encoder = pass.with(&self.pipelines.downsample);
            encoder.bind(
                0,
                &PassData {
                    params: values,
                    b_source: source_piece,
                    b_input: source_piece,
                    b_output: low_a,
                },
            );
            encoder.dispatch(groups_low);
        }
        if plan.has_blur() {
            {
                let mut pass = self.encoder.compute("backdrop.blur_h");
                let mut encoder = pass.with(&self.pipelines.blur_h);
                encoder.bind(
                    0,
                    &PassData {
                        params: values,
                        b_source: source_piece,
                        b_input: low_a,
                        b_output: low_b,
                    },
                );
                encoder.dispatch(groups_low);
            }
            {
                let mut pass = self.encoder.compute("backdrop.blur_v");
                let mut encoder = pass.with(&self.pipelines.blur_v);
                encoder.bind(
                    0,
                    &PassData {
                        params: values,
                        b_source: source_piece,
                        b_input: low_b,
                        b_output: low_a,
                    },
                );
                encoder.dispatch(groups_low);
            }
        }
        {
            let mut pass = self.encoder.compute("backdrop.composite");
            let mut encoder = pass.with(&self.pipelines.composite);
            encoder.bind(
                0,
                &PassData {
                    params: values,
                    b_source: source_piece,
                    b_input: low_a,
                    b_output: output,
                },
            );
            encoder.dispatch(groups_full);
        }
        {
            let mut transfer = self.encoder.transfer("backdrop.readback");
            transfer.copy_buffer_to_buffer(output, staging, region_bytes as u64);
        }

        let t_encode = t0.elapsed();
        let sync = self.ctx.submit(&mut self.encoder);
        let t_submit = t0.elapsed();
        if !self.ctx.wait_for(&sync, WAIT_TIMEOUT_MS) {
            self.disable();
            return Err(Error::Gpu("timed out waiting for the device".into()));
        }
        let t_wait = t0.elapsed();

        unsafe {
            let src_ptr = self.staging.host_ptr();
            debug_assert!(!src_ptr.is_null(), "shared memory must be mapped");
            std::ptr::copy_nonoverlapping(src_ptr, out.as_mut_ptr(), region_bytes);
        }
        if debug {
            let t_read = t0.elapsed();
            eprintln!(
                "[backdrop] upload {:.3}ms encode {:.3}ms submit {:.3}ms wait {:.3}ms readback {:.3}ms total {:.3}ms",
                t_upload.as_secs_f64() * 1e3,
                (t_encode - t_upload).as_secs_f64() * 1e3,
                (t_submit - t_encode).as_secs_f64() * 1e3,
                (t_wait - t_submit).as_secs_f64() * 1e3,
                (t_read - t_wait).as_secs_f64() * 1e3,
                t_read.as_secs_f64() * 1e3,
            );
        }
        Ok(())
    }
}

impl Drop for GpuBackend {
    fn drop(&mut self) {
        // Blade requires every resource to be destroyed before its context is
        // dropped; doing it here keeps the driver quiet on teardown.
        self.source.destroy(&self.ctx);
        self.low_a.destroy(&self.ctx);
        self.low_b.destroy(&self.ctx);
        self.output.destroy(&self.ctx);
        self.staging.destroy(&self.ctx);
        self.ctx
            .destroy_compute_pipeline(&mut self.pipelines.downsample);
        self.ctx.destroy_compute_pipeline(&mut self.pipelines.blur_h);
        self.ctx.destroy_compute_pipeline(&mut self.pipelines.blur_v);
        self.ctx
            .destroy_compute_pipeline(&mut self.pipelines.composite);
        self.ctx.destroy_command_encoder(&mut self.encoder);
    }
}
