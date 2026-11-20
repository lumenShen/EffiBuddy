// Doing `if let` gives you nice scoping with passes/encoders
#![allow(irrefutable_let_patterns)]

use super::{BladeAtlas, BladeContext};
use crate::{
    Background, BackdropFilter, Bounds, DevicePixels, GpuSpecs, MonochromeSprite, Path, Point,
    PolychromeSprite, PrimitiveBatch, Quad, ScaledPixels, Scene, Shadow, Size, Underline,
};
use blade_graphics as gpu;
use blade_util::{BufferBelt, BufferBeltDescriptor};
use bytemuck::{Pod, Zeroable};
#[cfg(target_os = "macos")]
use media::core_video::CVMetalTextureCache;
use std::sync::Arc;

const MAX_FRAME_TIME_MS: u32 = 10000;
/// Backdrop blurs run at 1/4 resolution in each axis.
const BACKDROP_BLUR_DIVISOR: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlobalParams {
    viewport_size: [f32; 2],
    premultiplied_alpha: u32,
    pad: u32,
}

//Note: we can't use `Bounds` directly here because
// it doesn't implement Pod + Zeroable
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PodBounds {
    origin: [f32; 2],
    size: [f32; 2],
}

impl From<Bounds<ScaledPixels>> for PodBounds {
    fn from(bounds: Bounds<ScaledPixels>) -> Self {
        Self {
            origin: [bounds.origin.x.0, bounds.origin.y.0],
            size: [bounds.size.width.0, bounds.size.height.0],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SurfaceParams {
    bounds: PodBounds,
    content_mask: PodBounds,
}

#[derive(blade_macros::ShaderData)]
struct ShaderQuadsData {
    globals: GlobalParams,
    b_quads: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderShadowsData {
    globals: GlobalParams,
    b_shadows: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPathRasterizationData {
    globals: GlobalParams,
    b_path_vertices: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPathsData {
    globals: GlobalParams,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_path_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderUnderlinesData {
    globals: GlobalParams,
    b_underlines: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderMonoSpritesData {
    globals: GlobalParams,
    gamma_ratios: [f32; 4],
    grayscale_enhanced_contrast: f32,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_mono_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPolySpritesData {
    globals: GlobalParams,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_poly_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderSurfacesData {
    globals: GlobalParams,
    surface_locals: SurfaceParams,
    t_y: gpu::TextureView,
    t_cb_cr: gpu::TextureView,
    s_surface: gpu::Sampler,
}

/// Parameters for compositing the blurred backdrop under a quad.
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default)]
#[repr(C)]
struct BackdropParams {
    uv_scale: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    pad: [f32; 2],
}

/// Parameters for one separable blur direction.
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C)]
struct BlurParams {
    uv_scale: [f32; 2],
    tap_step: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    weights: [f32; 12],
}

impl Default for BlurParams {
    fn default() -> Self {
        Self {
            uv_scale: [0.0; 2],
            tap_step: [0.0; 2],
            uv_min: [0.0; 2],
            uv_max: [1.0; 2],
            weights: [0.0; 12],
        }
    }
}

#[derive(blade_macros::ShaderData)]
struct ShaderBackdropData {
    globals: GlobalParams,
    backdrop: BackdropParams,
    t_backdrop: gpu::TextureView,
    s_backdrop: gpu::Sampler,
    b_quads: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderBackdropBlurData {
    blur: BlurParams,
    t_blur_source: gpu::TextureView,
    s_blur_source: gpu::Sampler,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug)]
#[repr(C)]
struct PathRasterizationVertex {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

struct BladePipelines {
    quads: gpu::RenderPipeline,
    backdrop: gpu::RenderPipeline,
    backdrop_blur: gpu::RenderPipeline,
    shadows: gpu::RenderPipeline,
    path_rasterization: gpu::RenderPipeline,
    paths: gpu::RenderPipeline,
    underlines: gpu::RenderPipeline,
    mono_sprites: gpu::RenderPipeline,
    poly_sprites: gpu::RenderPipeline,
    surfaces: gpu::RenderPipeline,
}

impl BladePipelines {
    fn new(gpu: &gpu::Context, surface_info: gpu::SurfaceInfo, path_sample_count: u32) -> Self {
        use gpu::ShaderData as _;

        log::info!(
            "Initializing Blade pipelines for surface {:?}",
            surface_info
        );
        let shader = gpu.create_shader(gpu::ShaderDesc {
            source: include_str!("shaders.wgsl"),
        });
        shader.check_struct_size::<GlobalParams>();
        shader.check_struct_size::<SurfaceParams>();
        shader.check_struct_size::<Quad>();
        shader.check_struct_size::<Shadow>();
        shader.check_struct_size::<PathRasterizationVertex>();
        shader.check_struct_size::<PathSprite>();
        shader.check_struct_size::<Underline>();
        shader.check_struct_size::<MonochromeSprite>();
        shader.check_struct_size::<PolychromeSprite>();

        // See https://apoorvaj.io/alpha-compositing-opengl-blending-and-premultiplied-alpha/
        let blend_mode = match surface_info.alpha {
            gpu::AlphaMode::Ignored => gpu::BlendState::ALPHA_BLENDING,
            gpu::AlphaMode::PreMultiplied => gpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            gpu::AlphaMode::PostMultiplied => gpu::BlendState::ALPHA_BLENDING,
        };
        let color_targets = &[gpu::ColorTargetState {
            format: surface_info.format,
            blend: Some(blend_mode),
            write_mask: gpu::ColorWrites::default(),
        }];

        Self {
            quads: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "quads",
                data_layouts: &[&ShaderQuadsData::layout()],
                vertex: shader.at("vs_quad"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_quad")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            // Composites the blurred backdrop through a quad's tint/rounding.
            backdrop: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "backdrop",
                data_layouts: &[&ShaderBackdropData::layout()],
                vertex: shader.at("vs_quad"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_backdrop")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            // Separable blur passes overwrite their scissored region: no blend.
            backdrop_blur: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "backdrop_blur",
                data_layouts: &[&ShaderBackdropBlurData::layout()],
                vertex: shader.at("vs_fullscreen"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_blur")),
                color_targets: &[gpu::ColorTargetState {
                    format: surface_info.format,
                    blend: None,
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState::default(),
            }),
            shadows: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "shadows",
                data_layouts: &[&ShaderShadowsData::layout()],
                vertex: shader.at("vs_shadow"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_shadow")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            path_rasterization: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "path_rasterization",
                data_layouts: &[&ShaderPathRasterizationData::layout()],
                vertex: shader.at("vs_path_rasterization"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_path_rasterization")),
                // The original implementation was using ADDITIVE blende mode,
                // I don't know why
                // color_targets: &[gpu::ColorTargetState {
                //     format: PATH_TEXTURE_FORMAT,
                //     blend: Some(gpu::BlendState::ADDITIVE),
                //     write_mask: gpu::ColorWrites::default(),
                // }],
                color_targets: &[gpu::ColorTargetState {
                    format: surface_info.format,
                    blend: Some(gpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState {
                    sample_count: path_sample_count,
                    ..Default::default()
                },
            }),
            paths: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "paths",
                data_layouts: &[&ShaderPathsData::layout()],
                vertex: shader.at("vs_path"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_path")),
                color_targets: &[gpu::ColorTargetState {
                    format: surface_info.format,
                    blend: Some(gpu::BlendState {
                        color: gpu::BlendComponent::OVER,
                        alpha: gpu::BlendComponent::ADDITIVE,
                    }),
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState::default(),
            }),
            underlines: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "underlines",
                data_layouts: &[&ShaderUnderlinesData::layout()],
                vertex: shader.at("vs_underline"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_underline")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            mono_sprites: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "mono-sprites",
                data_layouts: &[&ShaderMonoSpritesData::layout()],
                vertex: shader.at("vs_mono_sprite"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_mono_sprite")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            poly_sprites: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "poly-sprites",
                data_layouts: &[&ShaderPolySpritesData::layout()],
                vertex: shader.at("vs_poly_sprite"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_poly_sprite")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            surfaces: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "surfaces",
                data_layouts: &[&ShaderSurfacesData::layout()],
                vertex: shader.at("vs_surface"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_surface")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
        }
    }

    fn destroy(&mut self, gpu: &gpu::Context) {
        gpu.destroy_render_pipeline(&mut self.quads);
        gpu.destroy_render_pipeline(&mut self.backdrop);
        gpu.destroy_render_pipeline(&mut self.backdrop_blur);
        gpu.destroy_render_pipeline(&mut self.shadows);
        gpu.destroy_render_pipeline(&mut self.path_rasterization);
        gpu.destroy_render_pipeline(&mut self.paths);
        gpu.destroy_render_pipeline(&mut self.underlines);
        gpu.destroy_render_pipeline(&mut self.mono_sprites);
        gpu.destroy_render_pipeline(&mut self.poly_sprites);
        gpu.destroy_render_pipeline(&mut self.surfaces);
    }
}

pub struct BladeSurfaceConfig {
    pub size: gpu::Extent,
    pub transparent: bool,
    /// Vertical sync (presentation pacing) for this surface.
    ///
    /// `true` (the default) selects [`gpu::DisplaySync::Block`], i.e. Vulkan
    /// `FIFO` / Metal `displaySyncEnabled`: presenting blocks until the
    /// display's next vblank, so the render loop is capped at the monitor's
    /// refresh rate and never burns GPU/CPU on frames the user cannot see.
    ///
    /// `false` selects [`gpu::DisplaySync::Recent`] (Vulkan `MAILBOX`), which
    /// never blocks: useful for latency-sensitive work and for the perf lab,
    /// but it lets an animation loop run at several hundred fps.
    pub vsync: bool,
}

impl BladeSurfaceConfig {
    /// Map the vsync preference onto a blade display-sync mode.
    fn display_sync(&self) -> gpu::DisplaySync {
        if self.vsync {
            gpu::DisplaySync::Block
        } else {
            gpu::DisplaySync::Recent
        }
    }
}

/// Default `vsync` for a freshly created surface.
///
/// Historically this vendored renderer hard-coded `DisplaySync::Recent`, which
/// let animation frames race far past the monitor's refresh rate (400+ fps
/// while a panel slide or typewriter reveal is running). Frames are now paced
/// to the display by default; `EFFIBUDDY_VSYNC=0` restores the old unlocked
/// behaviour for the performance lab, which needs to measure raw throughput.
pub fn default_vsync() -> bool {
    static DEFAULT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEFAULT.get_or_init(|| {
        std::env::var("EFFIBUDDY_VSYNC").map_or(true, |value| value != "0")
    })
}

/// Report the presentation pacing chosen for a new surface.
///
/// Gated behind `EFFIBUDDY_PERF=1` because there is no logger installed in the
/// release binary — `log::info!` from the blade backend goes nowhere, so the
/// perf lab would otherwise have no way to tell a paced run from an unlocked
/// one. `Block` maps to Vulkan `FIFO` / Metal `displaySyncEnabled`, which syncs
/// with whatever the monitor actually runs at (60Hz, 120Hz, 144Hz, 240Hz, or a
/// VRR range) — never a hard-coded rate.
fn report_pacing(display_sync: gpu::DisplaySync) {
    if std::env::var("EFFIBUDDY_PERF").is_ok_and(|value| value == "1") {
        let mode = match display_sync {
            gpu::DisplaySync::Block => "FIFO (synced to display refresh)",
            gpu::DisplaySync::Recent => "MAILBOX (unlocked, may fall back)",
            gpu::DisplaySync::Tear => "IMMEDIATE (tearing)",
        };
        eprintln!("[gpu] requested surface display sync: {mode}");
    }
}

//Note: we could see some of these fields moved into `BladeContext`
// so that they are shared between windows. E.g. `pipelines`.
// But that is complicated by the fact that pipelines depend on
// the format and alpha mode.
pub struct BladeRenderer {
    gpu: Arc<gpu::Context>,
    surface: gpu::Surface,
    surface_config: gpu::SurfaceConfig,
    command_encoder: gpu::CommandEncoder,
    last_sync_point: Option<gpu::SyncPoint>,
    pipelines: BladePipelines,
    instance_belt: BufferBelt,
    atlas: Arc<BladeAtlas>,
    atlas_sampler: gpu::Sampler,
    #[cfg(target_os = "macos")]
    core_video_texture_cache: CVMetalTextureCache,
    path_intermediate_texture: gpu::Texture,
    path_intermediate_texture_view: gpu::TextureView,
    path_intermediate_msaa_texture: Option<gpu::Texture>,
    path_intermediate_msaa_texture_view: Option<gpu::TextureView>,
    /// Snapshot of the framebuffer used as the backdrop source.
    backdrop_capture: gpu::Texture,
    backdrop_capture_view: gpu::TextureView,
    /// Quarter-resolution ping-pong buffers for the separable blur.
    backdrop_blur_a: gpu::Texture,
    backdrop_blur_a_view: gpu::TextureView,
    backdrop_blur_b: gpu::Texture,
    backdrop_blur_b_view: gpu::TextureView,
    backdrop_blur_size: (u32, u32),
    rendering_parameters: RenderingParameters,
}

impl BladeRenderer {
    pub fn new<I: raw_window_handle::HasWindowHandle + raw_window_handle::HasDisplayHandle>(
        context: &BladeContext,
        window: &I,
        config: BladeSurfaceConfig,
    ) -> anyhow::Result<Self> {
        let surface_config = gpu::SurfaceConfig {
            size: config.size,
            // COPY lets the backdrop filter snapshot the frame.
            usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
            display_sync: config.display_sync(),
            color_space: gpu::ColorSpace::Srgb,
            allow_exclusive_full_screen: false,
            transparent: config.transparent,
        };
        let surface = context
            .gpu
            .create_surface_configured(window, surface_config)
            .map_err(|err| anyhow::anyhow!("Failed to create surface: {err:?}"))?;
        report_pacing(surface_config.display_sync);

        let command_encoder = context.gpu.create_command_encoder(gpu::CommandEncoderDesc {
            name: "main",
            buffer_count: 2,
        });
        let rendering_parameters = RenderingParameters::from_env(context);
        let pipelines = BladePipelines::new(
            &context.gpu,
            surface.info(),
            rendering_parameters.path_sample_count,
        );
        let instance_belt = BufferBelt::new(BufferBeltDescriptor {
            memory: gpu::Memory::Shared,
            min_chunk_size: 0x1000,
            alignment: 0x40, // Vulkan `minStorageBufferOffsetAlignment` on Intel Xe
        });
        let atlas = Arc::new(BladeAtlas::new(&context.gpu));
        let atlas_sampler = context.gpu.create_sampler(gpu::SamplerDesc {
            name: "path rasterization sampler",
            mag_filter: gpu::FilterMode::Linear,
            min_filter: gpu::FilterMode::Linear,
            ..Default::default()
        });

        let (path_intermediate_texture, path_intermediate_texture_view) =
            create_path_intermediate_texture(
                &context.gpu,
                surface.info().format,
                config.size.width,
                config.size.height,
            );
        let (backdrop_capture, backdrop_capture_view) = create_backdrop_textures(
            &context.gpu,
            "backdrop capture",
            surface.info().format,
            config.size.width,
            config.size.height,
        );
        let backdrop_blur_size = (
            config.size.width.div_ceil(BACKDROP_BLUR_DIVISOR).max(1),
            config.size.height.div_ceil(BACKDROP_BLUR_DIVISOR).max(1),
        );
        let (backdrop_blur_a, backdrop_blur_a_view) = create_backdrop_textures(
            &context.gpu,
            "backdrop blur a",
            surface.info().format,
            backdrop_blur_size.0,
            backdrop_blur_size.1,
        );
        let (backdrop_blur_b, backdrop_blur_b_view) = create_backdrop_textures(
            &context.gpu,
            "backdrop blur b",
            surface.info().format,
            backdrop_blur_size.0,
            backdrop_blur_size.1,
        );

        let (path_intermediate_msaa_texture, path_intermediate_msaa_texture_view) =
            create_msaa_texture_if_needed(
                &context.gpu,
                surface.info().format,
                config.size.width,
                config.size.height,
                rendering_parameters.path_sample_count,
            )
            .unzip();

        #[cfg(target_os = "macos")]
        let core_video_texture_cache = unsafe {
            CVMetalTextureCache::new(
                objc2::rc::Retained::as_ptr(&context.gpu.metal_device()) as *mut _
            )
            .unwrap()
        };

        Ok(Self {
            gpu: Arc::clone(&context.gpu),
            surface,
            surface_config,
            command_encoder,
            last_sync_point: None,
            pipelines,
            instance_belt,
            atlas,
            atlas_sampler,
            #[cfg(target_os = "macos")]
            core_video_texture_cache,
            path_intermediate_texture,
            path_intermediate_texture_view,
            path_intermediate_msaa_texture,
            path_intermediate_msaa_texture_view,
            backdrop_capture,
            backdrop_capture_view,
            backdrop_blur_a,
            backdrop_blur_a_view,
            backdrop_blur_b,
            backdrop_blur_b_view,
            backdrop_blur_size,
            rendering_parameters,
        })
    }

    fn wait_for_gpu(&mut self) {
        // EFFIBUDDY_GPU_SLOW_MS: emulate a low-performance GPU by extending
        // the real per-frame GPU wait. The extra stall lands exactly where a
        // slow GPU bites (the main thread parked in the present/fence wait),
        // so frame pacing and event-loop starvation behave like real
        // degraded hardware. CPU stays free, like a genuine GPU wait.
        if let Ok(extra_ms) = std::env::var("EFFIBUDDY_GPU_SLOW_MS")
            && let Ok(extra) = extra_ms.parse::<u64>()
            && extra > 0
        {
            std::thread::sleep(std::time::Duration::from_millis(extra));
        }
        if let Some(last_sp) = self.last_sync_point.take()
            && !self.gpu.wait_for(&last_sp, MAX_FRAME_TIME_MS)
        {
            log::error!("GPU hung");
            #[cfg(target_os = "linux")]
            if self.gpu.device_information().driver_name == "radv" {
                log::error!(
                    "there's a known bug with amdgpu/radv, try setting ZED_PATH_SAMPLE_COUNT=0 as a workaround"
                );
                log::error!(
                    "if that helps you're running into https://github.com/zed-industries/zed/issues/26143"
                );
            }
            log::error!(
                "your device information is: {:?}",
                self.gpu.device_information()
            );
            while !self.gpu.wait_for(&last_sp, MAX_FRAME_TIME_MS) {}
        }
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        self.update_drawable_size_impl(size, false);
    }

    /// Like `update_drawable_size` but skips the check that the size has changed. This is useful in
    /// cases like restoring a window from minimization where the size is the same but the
    /// renderer's swap chain needs to be recreated.
    #[cfg_attr(
        any(target_os = "macos", target_os = "linux", target_os = "freebsd"),
        allow(dead_code)
    )]
    pub fn update_drawable_size_even_if_unchanged(&mut self, size: Size<DevicePixels>) {
        self.update_drawable_size_impl(size, true);
    }

    fn update_drawable_size_impl(&mut self, size: Size<DevicePixels>, always_resize: bool) {
        let gpu_size = gpu::Extent {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth: 1,
        };

        if always_resize || gpu_size != self.surface_config.size {
            self.wait_for_gpu();
            self.surface_config.size = gpu_size;
            self.gpu
                .reconfigure_surface(&mut self.surface, self.surface_config);
            self.gpu.destroy_texture(self.path_intermediate_texture);
            self.gpu
                .destroy_texture_view(self.path_intermediate_texture_view);
            if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
                self.gpu.destroy_texture(msaa_texture);
            }
            if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
                self.gpu.destroy_texture_view(msaa_view);
            }
            let (path_intermediate_texture, path_intermediate_texture_view) =
                create_path_intermediate_texture(
                    &self.gpu,
                    self.surface.info().format,
                    gpu_size.width,
                    gpu_size.height,
                );
            self.path_intermediate_texture = path_intermediate_texture;
            self.path_intermediate_texture_view = path_intermediate_texture_view;
            let (path_intermediate_msaa_texture, path_intermediate_msaa_texture_view) =
                create_msaa_texture_if_needed(
                    &self.gpu,
                    self.surface.info().format,
                    gpu_size.width,
                    gpu_size.height,
                    self.rendering_parameters.path_sample_count,
                )
                .unzip();
            self.path_intermediate_msaa_texture = path_intermediate_msaa_texture;
            self.path_intermediate_msaa_texture_view = path_intermediate_msaa_texture_view;

            self.gpu.destroy_texture_view(self.backdrop_capture_view);
            self.gpu.destroy_texture(self.backdrop_capture);
            self.gpu.destroy_texture_view(self.backdrop_blur_a_view);
            self.gpu.destroy_texture(self.backdrop_blur_a);
            self.gpu.destroy_texture_view(self.backdrop_blur_b_view);
            self.gpu.destroy_texture(self.backdrop_blur_b);
            let (capture, capture_view) = create_backdrop_textures(
                &self.gpu,
                "backdrop capture",
                self.surface.info().format,
                gpu_size.width,
                gpu_size.height,
            );
            self.backdrop_capture = capture;
            self.backdrop_capture_view = capture_view;
            self.backdrop_blur_size = (
                gpu_size.width.div_ceil(BACKDROP_BLUR_DIVISOR).max(1),
                gpu_size.height.div_ceil(BACKDROP_BLUR_DIVISOR).max(1),
            );
            let (blur_a, blur_a_view) = create_backdrop_textures(
                &self.gpu,
                "backdrop blur a",
                self.surface.info().format,
                self.backdrop_blur_size.0,
                self.backdrop_blur_size.1,
            );
            self.backdrop_blur_a = blur_a;
            self.backdrop_blur_a_view = blur_a_view;
            let (blur_b, blur_b_view) = create_backdrop_textures(
                &self.gpu,
                "backdrop blur b",
                self.surface.info().format,
                self.backdrop_blur_size.0,
                self.backdrop_blur_size.1,
            );
            self.backdrop_blur_b = blur_b;
            self.backdrop_blur_b_view = blur_b_view;
        }
    }

    pub fn update_transparency(&mut self, transparent: bool) {
        if transparent != self.surface_config.transparent {
            self.wait_for_gpu();
            self.surface_config.transparent = transparent;
            self.gpu
                .reconfigure_surface(&mut self.surface, self.surface_config);
            self.pipelines.destroy(&self.gpu);
            self.pipelines = BladePipelines::new(
                &self.gpu,
                self.surface.info(),
                self.rendering_parameters.path_sample_count,
            );
        }
    }

    /// Enable or disable vertical sync for this surface.
    ///
    /// Reconfigures the swap chain in place, so it takes effect on the next
    /// presented frame without reopening the window. `true` caps the render
    /// loop at the display refresh rate (`FIFO` / `displaySyncEnabled`),
    /// `false` restores the unlocked `MAILBOX` path used by the perf lab.
    pub fn update_vsync(&mut self, vsync: bool) {
        let display_sync = if vsync {
            gpu::DisplaySync::Block
        } else {
            gpu::DisplaySync::Recent
        };
        if display_sync != self.surface_config.display_sync {
            self.wait_for_gpu();
            self.surface_config.display_sync = display_sync;
            self.gpu
                .reconfigure_surface(&mut self.surface, self.surface_config);
            report_pacing(display_sync);
        }
    }

    /// Whether the surface currently presents in sync with the display.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn vsync_enabled(&self) -> bool {
        self.surface_config.display_sync == gpu::DisplaySync::Block
    }

    #[cfg_attr(
        any(target_os = "macos", feature = "wayland", target_os = "windows"),
        allow(dead_code)
    )]
    pub fn viewport_size(&self) -> gpu::Extent {
        self.surface_config.size
    }

    pub fn sprite_atlas(&self) -> &Arc<BladeAtlas> {
        &self.atlas
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn gpu_specs(&self) -> GpuSpecs {
        let info = self.gpu.device_information();

        GpuSpecs {
            is_software_emulated: info.is_software_emulated,
            device_name: info.device_name.clone(),
            driver_name: info.driver_name.clone(),
            driver_info: info.driver_info.clone(),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn layer(&self) -> metal::MetalLayer {
        unsafe { foreign_types::ForeignType::from_ptr(self.layer_ptr()) }
    }

    #[cfg(target_os = "macos")]
    pub fn layer_ptr(&self) -> *mut metal::CAMetalLayer {
        objc2::rc::Retained::as_ptr(&self.surface.metal_layer()) as *mut _
    }

    #[profiling::function]
    fn draw_paths_to_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        width: f32,
        height: f32,
    ) {
        self.command_encoder
            .init_texture(self.path_intermediate_texture);
        if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
            self.command_encoder.init_texture(msaa_texture);
        }

        let target = if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
            gpu::RenderTarget {
                view: msaa_view,
                init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                finish_op: gpu::FinishOp::ResolveTo(self.path_intermediate_texture_view),
            }
        } else {
            gpu::RenderTarget {
                view: self.path_intermediate_texture_view,
                init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                finish_op: gpu::FinishOp::Store,
            }
        };
        if let mut pass = self.command_encoder.render(
            "rasterize paths",
            gpu::RenderTargetSet {
                colors: &[target],
                depth_stencil: None,
            },
        ) {
            let globals = GlobalParams {
                viewport_size: [width, height],
                premultiplied_alpha: 0,
                pad: 0,
            };
            let mut encoder = pass.with(&self.pipelines.path_rasterization);

            let mut vertices = Vec::new();
            for path in paths {
                vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                    xy_position: v.xy_position,
                    st_position: v.st_position,
                    color: path.color,
                    bounds: path.clipped_bounds(),
                }));
            }
            let vertex_buf = unsafe { self.instance_belt.alloc_typed(&vertices, &self.gpu) };
            encoder.bind(
                0,
                &ShaderPathRasterizationData {
                    globals,
                    b_path_vertices: vertex_buf,
                },
            );
            encoder.draw(0, vertices.len() as u32, 0, 1);
        }
    }

    pub fn destroy(&mut self) {
        self.wait_for_gpu();
        self.atlas.destroy();
        self.gpu.destroy_sampler(self.atlas_sampler);
        self.instance_belt.destroy(&self.gpu);
        self.gpu.destroy_command_encoder(&mut self.command_encoder);
        self.pipelines.destroy(&self.gpu);
        self.gpu.destroy_surface(&mut self.surface);
        self.gpu.destroy_texture(self.path_intermediate_texture);
        self.gpu
            .destroy_texture_view(self.path_intermediate_texture_view);
        if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
            self.gpu.destroy_texture(msaa_texture);
        }
        if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
            self.gpu.destroy_texture_view(msaa_view);
        }
        self.gpu.destroy_texture_view(self.backdrop_capture_view);
        self.gpu.destroy_texture(self.backdrop_capture);
        self.gpu.destroy_texture_view(self.backdrop_blur_a_view);
        self.gpu.destroy_texture(self.backdrop_blur_a);
        self.gpu.destroy_texture_view(self.backdrop_blur_b_view);
        self.gpu.destroy_texture(self.backdrop_blur_b);
    }

    /// Snapshot the framebuffer through `filter`, then blur it with two
    /// separable passes into `backdrop_blur_b`. Called with the main render
    /// pass ended, right before the element's own geometry is drawn.
    fn capture_and_blur_backdrop(
        &mut self,
        filter: &BackdropFilter,
        target_texture: gpu::Texture,
        target_view: gpu::TextureView,
    ) {
        let window_width = self.surface_config.size.width.max(1);
        let window_height = self.surface_config.size.height.max(1);
        let (blur_width, blur_height) = self.backdrop_blur_size;
        let format = self.surface.info().format;

        let region = backdrop_region(filter, window_width, window_height);
        let Some((origin_x, origin_y, width, height)) = region else {
            return;
        };
        let _ = format;

        self.command_encoder.init_texture(self.backdrop_capture);

        {
            let mut transfer = self.command_encoder.transfer("backdrop capture");
            transfer.copy_texture_to_texture(
                gpu::TexturePiece {
                    texture: target_texture,
                    mip_level: 0,
                    array_layer: 0,
                    origin: [origin_x, origin_y, 0],
                },
                gpu::TexturePiece {
                    texture: self.backdrop_capture,
                    mip_level: 0,
                    array_layer: 0,
                    origin: [origin_x, origin_y, 0],
                },
                gpu::Extent {
                    width,
                    height,
                    depth: 1,
                },
            );
        }

        // Region in blur-texture texels.
        let blur_x = origin_x / BACKDROP_BLUR_DIVISOR;
        let blur_y = origin_y / BACKDROP_BLUR_DIVISOR;
        let blur_w = width.div_ceil(BACKDROP_BLUR_DIVISOR).max(1);
        let blur_h = height.div_ceil(BACKDROP_BLUR_DIVISOR).max(1);

        let weights = backdrop_blur_weights(filter.blur_radius);
        let window_uv = [
            BACKDROP_BLUR_DIVISOR as f32 / window_width as f32,
            BACKDROP_BLUR_DIVISOR as f32 / window_height as f32,
        ];
        let blur_uv = [1.0 / blur_width as f32, 1.0 / blur_height as f32];

        // Horizontal pass: capture -> blur_a.
        self.command_encoder.init_texture(self.backdrop_blur_a);
        let horizontal = BlurParams {
            uv_scale: window_uv,
            tap_step: [window_uv[0], 0.0],
            uv_min: [
                (origin_x as f32 + 0.5) / window_width as f32,
                (origin_y as f32 + 0.5) / window_height as f32,
            ],
            uv_max: [
                ((origin_x + width) as f32 - 0.5) / window_width as f32,
                ((origin_y + height) as f32 - 0.5) / window_height as f32,
            ],
            weights,
        };
        if let mut pass = self.command_encoder.render(
            "backdrop blur x",
            gpu::RenderTargetSet {
                colors: &[gpu::RenderTarget {
                    view: self.backdrop_blur_a_view,
                    init_op: gpu::InitOp::Load,
                    finish_op: gpu::FinishOp::Store,
                }],
                depth_stencil: None,
            },
        ) {
            pass.set_viewport(&gpu::Viewport {
                x: blur_x as f32,
                y: blur_y as f32,
                w: blur_w as f32,
                h: blur_h as f32,
                depth: 0.0..1.0,
            });
            pass.set_scissor_rect(&gpu::ScissorRect {
                x: blur_x as i32,
                y: blur_y as i32,
                w: blur_w,
                h: blur_h,
            });
            let mut encoder = pass.with(&self.pipelines.backdrop_blur);
            encoder.bind(
                0,
                &ShaderBackdropBlurData {
                    blur: horizontal,
                    t_blur_source: self.backdrop_capture_view,
                    s_blur_source: self.atlas_sampler,
                },
            );
            encoder.draw(0, 3, 0, 1);
        }

        // Vertical pass: blur_a -> blur_b.
        self.command_encoder.init_texture(self.backdrop_blur_b);
        let vertical = BlurParams {
            uv_scale: blur_uv,
            tap_step: [0.0, blur_uv[1]],
            uv_min: [
                (blur_x as f32 + 0.5) / blur_width as f32,
                (blur_y as f32 + 0.5) / blur_height as f32,
            ],
            uv_max: [
                ((blur_x + blur_w) as f32 - 0.5) / blur_width as f32,
                ((blur_y + blur_h) as f32 - 0.5) / blur_height as f32,
            ],
            weights,
        };
        if let mut pass = self.command_encoder.render(
            "backdrop blur y",
            gpu::RenderTargetSet {
                colors: &[gpu::RenderTarget {
                    view: self.backdrop_blur_b_view,
                    init_op: gpu::InitOp::Load,
                    finish_op: gpu::FinishOp::Store,
                }],
                depth_stencil: None,
            },
        ) {
            pass.set_viewport(&gpu::Viewport {
                x: blur_x as f32,
                y: blur_y as f32,
                w: blur_w as f32,
                h: blur_h as f32,
                depth: 0.0..1.0,
            });
            pass.set_scissor_rect(&gpu::ScissorRect {
                x: blur_x as i32,
                y: blur_y as i32,
                w: blur_w,
                h: blur_h,
            });
            let mut encoder = pass.with(&self.pipelines.backdrop_blur);
            encoder.bind(
                0,
                &ShaderBackdropBlurData {
                    blur: vertical,
                    t_blur_source: self.backdrop_blur_a_view,
                    s_blur_source: self.atlas_sampler,
                },
            );
            encoder.draw(0, 3, 0, 1);
        }

        let _ = target_view;
    }

    /// Composite parameters for the final glass quad.
    fn backdrop_composite_params(&self, filter: &BackdropFilter) -> BackdropParams {
        let window_width = self.surface_config.size.width.max(1);
        let window_height = self.surface_config.size.height.max(1);
        let (blur_width, blur_height) = self.backdrop_blur_size;
        let (origin_x, origin_y, width, height) =
            backdrop_region(filter, window_width, window_height).unwrap_or((0, 0, 1, 1));
        let blur_x = origin_x / BACKDROP_BLUR_DIVISOR;
        let blur_y = origin_y / BACKDROP_BLUR_DIVISOR;
        let blur_w = width.div_ceil(BACKDROP_BLUR_DIVISOR).max(1);
        let blur_h = height.div_ceil(BACKDROP_BLUR_DIVISOR).max(1);

        BackdropParams {
            // Fragment positions are in window pixels; the blur texture holds
            // one texel per BLUR_DIVISOR pixels.
            uv_scale: [
                1.0 / (blur_width as f32 * BACKDROP_BLUR_DIVISOR as f32),
                1.0 / (blur_height as f32 * BACKDROP_BLUR_DIVISOR as f32),
            ],
            uv_min: [
                (blur_x as f32 + 0.5) / blur_width as f32,
                (blur_y as f32 + 0.5) / blur_height as f32,
            ],
            uv_max: [
                ((blur_x + blur_w) as f32 - 0.5) / blur_width as f32,
                ((blur_y + blur_h) as f32 - 0.5) / blur_height as f32,
            ],
            pad: [0.0; 2],
        }
    }

    pub fn draw(&mut self, scene: &Scene) {
        self.command_encoder.start();
        self.atlas.before_frame(&mut self.command_encoder);

        let frame = {
            profiling::scope!("acquire frame");
            self.surface.acquire_frame()
        };
        self.command_encoder.init_texture(frame.texture());

        let globals = GlobalParams {
            viewport_size: [
                self.surface_config.size.width as f32,
                self.surface_config.size.height as f32,
            ],
            premultiplied_alpha: match self.surface.info().alpha {
                gpu::AlphaMode::Ignored | gpu::AlphaMode::PostMultiplied => 0,
                gpu::AlphaMode::PreMultiplied => 1,
            },
            pad: 0,
        };

        let mut pass = self.command_encoder.render(
            "main",
            gpu::RenderTargetSet {
                colors: &[gpu::RenderTarget {
                    view: frame.texture_view(),
                    init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                    finish_op: gpu::FinishOp::Store,
                }],
                depth_stencil: None,
            },
        );

        profiling::scope!("render pass");
        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::BackdropFilters(filters) => {
                    for filter in filters {
                        // Snapshot + blur with the main pass closed, then
                        // reopen it and composite the glass quad.
                        drop(pass);
                        self.capture_and_blur_backdrop(
                            filter,
                            frame.texture(),
                            frame.texture_view(),
                        );

                        // Compute the uv mapping before `self` is borrowed by
                        // the render pass.
                        let params = self.backdrop_composite_params(filter);
                        pass = self.command_encoder.render(
                            "main",
                            gpu::RenderTargetSet {
                                colors: &[gpu::RenderTarget {
                                    view: frame.texture_view(),
                                    init_op: gpu::InitOp::Load,
                                    finish_op: gpu::FinishOp::Store,
                                }],
                                depth_stencil: None,
                            },
                        );

                        let quad = Quad {
                            order: filter.order,
                            border_style: Default::default(),
                            bounds: filter.bounds,
                            content_mask: filter.content_mask.clone(),
                            background: crate::solid_background(filter.tint),
                            border_color: Default::default(),
                            corner_radii: filter.corner_radii,
                            border_widths: Default::default(),
                        };
                        let instance_buf =
                            unsafe { self.instance_belt.alloc_typed(&[quad], &self.gpu) };
                        let mut encoder = pass.with(&self.pipelines.backdrop);
                        encoder.bind(
                            0,
                            &ShaderBackdropData {
                                globals,
                                backdrop: params,
                                t_backdrop: self.backdrop_blur_b_view,
                                s_backdrop: self.atlas_sampler,
                                b_quads: instance_buf,
                            },
                        );
                        encoder.draw(0, 4, 0, 1);
                    }
                }
                PrimitiveBatch::Quads(quads) => {
                    let instance_buf = unsafe { self.instance_belt.alloc_typed(quads, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.quads);
                    encoder.bind(
                        0,
                        &ShaderQuadsData {
                            globals,
                            b_quads: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, quads.len() as u32);
                }
                PrimitiveBatch::Shadows(shadows) => {
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(shadows, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.shadows);
                    encoder.bind(
                        0,
                        &ShaderShadowsData {
                            globals,
                            b_shadows: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, shadows.len() as u32);
                }
                PrimitiveBatch::Paths(paths) => {
                    let Some(first_path) = paths.first() else {
                        continue;
                    };
                    drop(pass);
                    self.draw_paths_to_intermediate(
                        paths,
                        self.surface_config.size.width as f32,
                        self.surface_config.size.height as f32,
                    );
                    pass = self.command_encoder.render(
                        "main",
                        gpu::RenderTargetSet {
                            colors: &[gpu::RenderTarget {
                                view: frame.texture_view(),
                                init_op: gpu::InitOp::Load,
                                finish_op: gpu::FinishOp::Store,
                            }],
                            depth_stencil: None,
                        },
                    );
                    let mut encoder = pass.with(&self.pipelines.paths);
                    // When copying paths from the intermediate texture to the drawable,
                    // each pixel must only be copied once, in case of transparent paths.
                    //
                    // If all paths have the same draw order, then their bounds are all
                    // disjoint, so we can copy each path's bounds individually. If this
                    // batch combines different draw orders, we perform a single copy
                    // for a minimal spanning rect.
                    let sprites = if paths.last().unwrap().order == first_path.order {
                        paths
                            .iter()
                            .map(|path| PathSprite {
                                bounds: path.clipped_bounds(),
                            })
                            .collect()
                    } else {
                        let mut bounds = first_path.clipped_bounds();
                        for path in paths.iter().skip(1) {
                            bounds = bounds.union(&path.clipped_bounds());
                        }
                        vec![PathSprite { bounds }]
                    };
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(&sprites, &self.gpu) };
                    encoder.bind(
                        0,
                        &ShaderPathsData {
                            globals,
                            t_sprite: self.path_intermediate_texture_view,
                            s_sprite: self.atlas_sampler,
                            b_path_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::Underlines(underlines) => {
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(underlines, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.underlines);
                    encoder.bind(
                        0,
                        &ShaderUnderlinesData {
                            globals,
                            b_underlines: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, underlines.len() as u32);
                }
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => {
                    let tex_info = self.atlas.get_texture_info(texture_id);
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(sprites, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.mono_sprites);
                    encoder.bind(
                        0,
                        &ShaderMonoSpritesData {
                            globals,
                            gamma_ratios: self.rendering_parameters.gamma_ratios,
                            grayscale_enhanced_contrast: self
                                .rendering_parameters
                                .grayscale_enhanced_contrast,
                            t_sprite: tex_info.raw_view,
                            s_sprite: self.atlas_sampler,
                            b_mono_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => {
                    let tex_info = self.atlas.get_texture_info(texture_id);
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(sprites, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.poly_sprites);
                    encoder.bind(
                        0,
                        &ShaderPolySpritesData {
                            globals,
                            t_sprite: tex_info.raw_view,
                            s_sprite: self.atlas_sampler,
                            b_poly_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::Surfaces(surfaces) => {
                    let mut _encoder = pass.with(&self.pipelines.surfaces);

                    for surface in surfaces {
                        #[cfg(not(target_os = "macos"))]
                        {
                            let _ = surface;
                            continue;
                        };

                        #[cfg(target_os = "macos")]
                        {
                            let (t_y, t_cb_cr) = unsafe {
                                use core_foundation::base::TCFType as _;
                                use std::ptr;

                                assert_eq!(
                                        surface.image_buffer.get_pixel_format(),
                                        core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                                    );

                                let y_texture = self
                                    .core_video_texture_cache
                                    .create_texture_from_image(
                                        surface.image_buffer.as_concrete_TypeRef(),
                                        ptr::null(),
                                        metal::MTLPixelFormat::R8Unorm,
                                        surface.image_buffer.get_width_of_plane(0),
                                        surface.image_buffer.get_height_of_plane(0),
                                        0,
                                    )
                                    .unwrap();
                                let cb_cr_texture = self
                                    .core_video_texture_cache
                                    .create_texture_from_image(
                                        surface.image_buffer.as_concrete_TypeRef(),
                                        ptr::null(),
                                        metal::MTLPixelFormat::RG8Unorm,
                                        surface.image_buffer.get_width_of_plane(1),
                                        surface.image_buffer.get_height_of_plane(1),
                                        1,
                                    )
                                    .unwrap();
                                (
                                    gpu::TextureView::from_metal_texture(
                                        &objc2::rc::Retained::retain(
                                            foreign_types::ForeignTypeRef::as_ptr(
                                                y_texture.as_texture_ref(),
                                            )
                                                as *mut objc2::runtime::ProtocolObject<
                                                    dyn objc2_metal::MTLTexture,
                                                >,
                                        )
                                        .unwrap(),
                                        gpu::TexelAspects::COLOR,
                                    ),
                                    gpu::TextureView::from_metal_texture(
                                        &objc2::rc::Retained::retain(
                                            foreign_types::ForeignTypeRef::as_ptr(
                                                cb_cr_texture.as_texture_ref(),
                                            )
                                                as *mut objc2::runtime::ProtocolObject<
                                                    dyn objc2_metal::MTLTexture,
                                                >,
                                        )
                                        .unwrap(),
                                        gpu::TexelAspects::COLOR,
                                    ),
                                )
                            };

                            _encoder.bind(
                                0,
                                &ShaderSurfacesData {
                                    globals,
                                    surface_locals: SurfaceParams {
                                        bounds: surface.bounds.into(),
                                        content_mask: surface.content_mask.bounds.into(),
                                    },
                                    t_y,
                                    t_cb_cr,
                                    s_surface: self.atlas_sampler,
                                },
                            );

                            _encoder.draw(0, 4, 0, 1);
                        }
                    }
                }
            }
        }
        drop(pass);

        self.command_encoder.present(frame);
        let sync_point = self.gpu.submit(&mut self.command_encoder);

        profiling::scope!("finish");
        self.instance_belt.flush(&sync_point);
        self.atlas.after_frame(&sync_point);

        self.wait_for_gpu();
        self.last_sync_point = Some(sync_point);
    }
}

/// Textures used by the backdrop filter: one full-size capture and two
/// quarter-resolution blur targets. All three are copy + sample + render
/// targets so the passes can hand off to each other.
fn create_backdrop_textures(
    gpu: &gpu::Context,
    name: &'static str,
    format: gpu::TextureFormat,
    width: u32,
    height: u32,
) -> (gpu::Texture, gpu::TextureView) {
    let texture = gpu.create_texture(gpu::TextureDesc {
        name,
        format,
        size: gpu::Extent {
            width: width.max(1),
            height: height.max(1),
            depth: 1,
        },
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: gpu::TextureDimension::D2,
        usage: gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE | gpu::TextureUsage::TARGET,
        external: None,
    });
    let view = gpu.create_texture_view(
        texture,
        gpu::TextureViewDesc {
            name,
            format,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );
    (texture, view)
}

/// Nine Gaussian weights for a blur of `radius` physical pixels, evaluated at
/// quarter resolution (the blur passes work at 1/4 scale, one tap per blur
/// pixel).
fn backdrop_blur_weights(radius: f32) -> [f32; 12] {
    let mut weights = [0.0f32; 12];
    let sigma = (radius * 0.5 / 4.0).clamp(0.0, 5.0);
    if sigma < 0.05 {
        weights[4] = 1.0;
        return weights;
    }
    let mut total = 0.0;
    for tap in -4i32..=4 {
        let weight = (-(tap * tap) as f32 / (2.0 * sigma * sigma)).exp();
        weights[(tap + 4) as usize] = weight;
        total += weight;
    }
    for weight in weights.iter_mut().take(9) {
        *weight /= total;
    }
    weights
}

/// The drawable-space rectangle a backdrop filter covers, as
/// `(x, y, width, height)` in whole pixels, or `None` when it is empty.
fn backdrop_region(
    filter: &BackdropFilter,
    window_width: u32,
    window_height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let left = filter.bounds.origin.x.0.floor().clamp(0.0, window_width as f32);
    let top = filter.bounds.origin.y.0.floor().clamp(0.0, window_height as f32);
    let right = (filter.bounds.origin.x.0 + filter.bounds.size.width.0)
        .ceil()
        .clamp(0.0, window_width as f32);
    let bottom = (filter.bounds.origin.y.0 + filter.bounds.size.height.0)
        .ceil()
        .clamp(0.0, window_height as f32);
    let width = (right - left) as u32;
    let height = (bottom - top) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some((left as u32, top as u32, width, height))
}

fn create_path_intermediate_texture(
    gpu: &gpu::Context,
    format: gpu::TextureFormat,
    width: u32,
    height: u32,
) -> (gpu::Texture, gpu::TextureView) {
    let texture = gpu.create_texture(gpu::TextureDesc {
        name: "path intermediate",
        format,
        size: gpu::Extent {
            width,
            height,
            depth: 1,
        },
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: gpu::TextureDimension::D2,
        usage: gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE | gpu::TextureUsage::TARGET,
        external: None,
    });
    let texture_view = gpu.create_texture_view(
        texture,
        gpu::TextureViewDesc {
            name: "path intermediate view",
            format,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );
    (texture, texture_view)
}

fn create_msaa_texture_if_needed(
    gpu: &gpu::Context,
    format: gpu::TextureFormat,
    width: u32,
    height: u32,
    sample_count: u32,
) -> Option<(gpu::Texture, gpu::TextureView)> {
    if sample_count <= 1 {
        return None;
    }
    let texture_msaa = gpu.create_texture(gpu::TextureDesc {
        name: "path intermediate msaa",
        format,
        size: gpu::Extent {
            width,
            height,
            depth: 1,
        },
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count,
        dimension: gpu::TextureDimension::D2,
        usage: gpu::TextureUsage::TARGET,
        external: None,
    });
    let texture_view_msaa = gpu.create_texture_view(
        texture_msaa,
        gpu::TextureViewDesc {
            name: "path intermediate msaa view",
            format,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );

    Some((texture_msaa, texture_view_msaa))
}

/// A set of parameters that can be set using a corresponding environment variable.
struct RenderingParameters {
    // Env var: ZED_PATH_SAMPLE_COUNT
    // workaround for https://github.com/zed-industries/zed/issues/26143
    path_sample_count: u32,

    // Env var: ZED_FONTS_GAMMA
    // Allowed range [1.0, 2.2], other values are clipped
    // Default: 1.8
    gamma_ratios: [f32; 4],
    // Env var: ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST
    // Allowed range: [0.0, ..), other values are clipped
    // Default: 1.0
    grayscale_enhanced_contrast: f32,
}

impl RenderingParameters {
    fn from_env(context: &BladeContext) -> Self {
        use std::env;

        let path_sample_count = env::var("ZED_PATH_SAMPLE_COUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                [4, 2, 1]
                    .into_iter()
                    .find(|&n| (context.gpu.capabilities().sample_count_mask & n) != 0)
            })
            .unwrap_or(1);
        let gamma = env::var("ZED_FONTS_GAMMA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.8_f32)
            .clamp(1.0, 2.2);
        let gamma_ratios = Self::get_gamma_ratios(gamma);
        let grayscale_enhanced_contrast = env::var("ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0_f32)
            .max(0.0);

        Self {
            path_sample_count,
            gamma_ratios,
            grayscale_enhanced_contrast,
        }
    }

    // Gamma ratios for brightening/darkening edges for better contrast
    // https://github.com/microsoft/terminal/blob/1283c0f5b99a2961673249fa77c6b986efb5086c/src/renderer/atlas/dwrite.cpp#L50
    fn get_gamma_ratios(gamma: f32) -> [f32; 4] {
        const GAMMA_INCORRECT_TARGET_RATIOS: [[f32; 4]; 13] = [
            [0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0], // gamma = 1.0
            [0.0166 / 4.0, -0.0807 / 4.0, 0.2227 / 4.0, -0.0751 / 4.0], // gamma = 1.1
            [0.0350 / 4.0, -0.1760 / 4.0, 0.4325 / 4.0, -0.1370 / 4.0], // gamma = 1.2
            [0.0543 / 4.0, -0.2821 / 4.0, 0.6302 / 4.0, -0.1876 / 4.0], // gamma = 1.3
            [0.0739 / 4.0, -0.3963 / 4.0, 0.8167 / 4.0, -0.2287 / 4.0], // gamma = 1.4
            [0.0933 / 4.0, -0.5161 / 4.0, 0.9926 / 4.0, -0.2616 / 4.0], // gamma = 1.5
            [0.1121 / 4.0, -0.6395 / 4.0, 1.1588 / 4.0, -0.2877 / 4.0], // gamma = 1.6
            [0.1300 / 4.0, -0.7649 / 4.0, 1.3159 / 4.0, -0.3080 / 4.0], // gamma = 1.7
            [0.1469 / 4.0, -0.8911 / 4.0, 1.4644 / 4.0, -0.3234 / 4.0], // gamma = 1.8
            [0.1627 / 4.0, -1.0170 / 4.0, 1.6051 / 4.0, -0.3347 / 4.0], // gamma = 1.9
            [0.1773 / 4.0, -1.1420 / 4.0, 1.7385 / 4.0, -0.3426 / 4.0], // gamma = 2.0
            [0.1908 / 4.0, -1.2652 / 4.0, 1.8650 / 4.0, -0.3476 / 4.0], // gamma = 2.1
            [0.2031 / 4.0, -1.3864 / 4.0, 1.9851 / 4.0, -0.3501 / 4.0], // gamma = 2.2
        ];

        const NORM13: f32 = ((0x10000 as f64) / (255.0 * 255.0) * 4.0) as f32;
        const NORM24: f32 = ((0x100 as f64) / (255.0) * 4.0) as f32;

        let index = ((gamma * 10.0).round() as usize).clamp(10, 22) - 10;
        let ratios = GAMMA_INCORRECT_TARGET_RATIOS[index];

        [
            ratios[0] * NORM13,
            ratios[1] * NORM24,
            ratios[2] * NORM13,
            ratios[3] * NORM24,
        ]
    }
}
