//! Frosted-glass backdrops: the design-system face of `effibuddy-backdrop`.
//!
//! A glass panel is "blur what is behind me, tint it, round my corners". GPUI
//! does not hand widgets the pixels underneath them, so this module gives the
//! two halves that *can* be reused today:
//!
//! * [`GlassStyle`] / [`BackdropParams`] presets that match the design tokens,
//! * [`GlassSurface`], a reusable CPU/GPU filter that turns any RGBA8 source
//!   region into the finished, tinted, corner-masked panel image,
//! * [`to_render_image`], the bridge from those bytes to a `gpui::RenderImage`
//!   so the result can be drawn with `img()`.
//!
//! Typical use for a backdrop you already own (a wallpaper, a chart snapshot, a
//! screenshot):
//!
//! ```ignore
//! let mut surface = GlassSurface::new(glass::DIALOG);
//! let pixels = surface.refresh(ImageRef::new(&wallpaper, w, h), Rect::new(560, 240, 800, 600))?;
//! // cache the RenderImage; rebuild only when the wallpaper or the panel moves
//! let image = glass::to_render_image(pixels, 800, 600);
//! ```

pub use effibuddy_backdrop::{
    BackdropFilter, BackdropParams, BackendKind, CornerRadius, FrameStats, ImageRef, Quality, Rect,
    Rgba8,
};

use std::sync::Arc;

use gpui::RenderImage;

/// A design-token description of a glass panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlassStyle {
    /// CSS-equivalent blur radius in physical pixels.
    pub blur: f32,
    pub saturation: f32,
    pub brightness: f32,
    pub contrast: f32,
    /// Overlay color; alpha is how "milky" the glass is.
    pub tint: Rgba8,
    pub corner: f32,
    pub quality: Quality,
}

impl GlassStyle {
    /// Convert to the filter's parameter block for a panel of `width` x
    /// `height` physical pixels.
    pub fn params(&self) -> BackdropParams {
        BackdropParams {
            blur_radius: self.blur,
            saturation: self.saturation,
            brightness: self.brightness,
            contrast: self.contrast,
            tint: self.tint,
            corners: CornerRadius::all(self.corner),
            quality: self.quality,
            ..BackdropParams::default()
        }
    }

    pub const fn with_corner(mut self, corner: f32) -> Self {
        self.corner = corner;
        self
    }

    pub const fn with_blur(mut self, blur: f32) -> Self {
        self.blur = blur;
        self
    }

    pub const fn with_tint(mut self, tint: Rgba8) -> Self {
        self.tint = tint;
        self
    }
}

/// Dialogs and modal cards: wider blur, a little lift in brightness.
pub const DIALOG: GlassStyle = GlassStyle {
    blur: 24.0,
    saturation: 1.18,
    brightness: 1.04,
    contrast: 0.98,
    tint: Rgba8::from_rgb_alpha(0xFFFFFF, 40),
    corner: crate::theme::radius::LG,
    quality: Quality::Balanced,
};

/// Popovers and menus: lighter, faster, tighter corners.
pub const MENU: GlassStyle = GlassStyle {
    blur: 18.0,
    saturation: 1.1,
    brightness: 1.03,
    contrast: 1.0,
    tint: Rgba8::from_rgb_alpha(0xFFFFFF, 64),
    corner: crate::theme::radius::MD,
    quality: Quality::Fast,
};

/// Side panes: strong blur, almost no tint.
pub const PANE: GlassStyle = GlassStyle {
    blur: 32.0,
    saturation: 1.05,
    brightness: 1.0,
    contrast: 1.0,
    tint: Rgba8::from_rgb_alpha(0xFFFFFF, 20),
    corner: crate::theme::radius::XXL,
    quality: Quality::Balanced,
};

/// Dark-theme variant of [`DIALOG`]: a black veil instead of a white one.
pub const DIALOG_DARK: GlassStyle = GlassStyle {
    blur: 24.0,
    saturation: 1.05,
    brightness: 0.96,
    contrast: 1.02,
    tint: Rgba8::from_rgb_alpha(0x101318, 96),
    corner: crate::theme::radius::LG,
    quality: Quality::Balanced,
};

/// A reusable backdrop-filter surface.
///
/// Owns the filter (including its GPU device and scratch buffers), the output
/// buffer and the last measured stats. A steady-state refresh allocates nothing.
pub struct GlassSurface {
    filter: BackdropFilter,
    pixels: Vec<u8>,
    style: GlassStyle,
    size: (u32, u32),
    dirty: bool,
}

impl GlassSurface {
    pub fn new(style: GlassStyle) -> Self {
        Self {
            filter: BackdropFilter::new(),
            pixels: Vec::new(),
            style,
            size: (0, 0),
            dirty: true,
        }
    }

    /// Pick the matching preset for the current theme mode.
    pub fn for_theme(style: GlassStyle, dark: bool) -> Self {
        if dark {
            Self::new(GlassStyle {
                tint: Rgba8::from_rgb_alpha(0x101318, style.tint.a.max(72)),
                brightness: 0.96,
                ..style
            })
        } else {
            Self::new(style)
        }
    }

    pub fn style(&self) -> GlassStyle {
        self.style
    }

    /// Change the look; the next refresh re-runs the filter.
    pub fn set_style(&mut self, style: GlassStyle) {
        if style != self.style {
            self.style = style;
            self.dirty = true;
        }
    }

    /// `true` when the last refresh produced pixels.
    pub fn has_pixels(&self) -> bool {
        !self.pixels.is_empty() && !self.dirty
    }

    /// The finished panel, tightly packed RGBA8 at `size()`.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Hold a frame budget: when the effect keeps exceeding it the filter drops
    /// to [`Quality::Fast`] until the machine is comfortable again.
    ///
    /// ```ignore
    /// surface.set_frame_budget(Some(Duration::from_secs_f64(1.0 / 300.0)));
    /// ```
    pub fn set_frame_budget(&mut self, budget: Option<std::time::Duration>) {
        self.filter.set_frame_budget(budget);
    }

    pub fn stats(&self) -> FrameStats {
        self.filter.stats()
    }

    pub fn backend(&self) -> BackendKind {
        self.filter.backend()
    }

    /// e.g. `"vulkan compute (blade)"` or `"cpu (rayon)"`.
    pub fn accelerator(&self) -> String {
        self.filter.accelerator()
    }

    /// Blur `region` out of `source` and return the finished panel.
    ///
    /// Calling this every frame is fine (it is measured in tenths of a
    /// millisecond for panel-sized regions); call it only when the source or
    /// the panel geometry changed if the backdrop is static.
    pub fn refresh(
        &mut self,
        source: ImageRef<'_>,
        region: Rect,
    ) -> Result<&[u8], effibuddy_backdrop::Error> {
        let params = self.style.params();
        self.filter
            .apply_into_vec(&source, region, &params, &mut self.pixels)?;
        self.size = (region.width, region.height);
        self.dirty = false;
        Ok(&self.pixels)
    }

    /// Mark the cached pixels stale (source changed underneath us).
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Bridge to GPUI: wrap the current pixels as a `RenderImage`.
    ///
    /// Returns `None` before the first refresh. GPUI's renderer expects BGRA
    /// (that is what its image loader produces), so the channels are swapped
    /// here once per rebuild.
    pub fn render_image(&self) -> Option<Arc<RenderImage>> {
        if self.dirty {
            return None;
        }
        to_render_image(&self.pixels, self.size.0, self.size.1)
    }
}

/// Convert tightly packed RGBA8 pixels into a GPUI image (BGRA under the hood).
pub fn to_render_image(rgba: &[u8], width: u32, height: u32) -> Option<Arc<RenderImage>> {
    if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
        return None;
    }
    let mut bgra = rgba[..width as usize * height as usize * 4].to_vec();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let index = (y * width as usize + x) * 4;
                data[index] = (x * 255 / width as usize) as u8;
                data[index + 1] = (y * 255 / height as usize) as u8;
                data[index + 2] = 200;
                data[index + 3] = 255;
            }
        }
        data
    }

    #[test]
    fn refresh_produces_a_corner_masked_panel() {
        let source = gradient(200, 160);
        let mut surface = GlassSurface::new(DIALOG.with_corner(16.0));
        let pixels = surface
            .refresh(ImageRef::new(&source, 200, 160), Rect::new(40, 20, 120, 100))
            .unwrap()
            .to_vec();
        assert_eq!(pixels.len(), 120 * 100 * 4);
        assert_eq!(pixels[3], 0, "rounded corner must be transparent");
        let center = (50 * 120 + 60) * 4;
        assert_eq!(pixels[center + 3], 255, "the middle stays opaque");
        assert_eq!(surface.size(), (120, 100));
        assert!(surface.has_pixels());
    }

    #[test]
    fn tint_lightens_the_panel() {
        let source = vec![0u8; 64 * 64 * 4];
        let dark = GlassStyle {
            blur: 0.0,
            tint: Rgba8::TRANSPARENT,
            ..DIALOG.with_corner(0.0)
        };
        let light = GlassStyle {
            tint: Rgba8::opaque(255, 255, 255),
            ..dark
        };

        let mut plain = GlassSurface::new(dark);
        let plain_pixels = plain
            .refresh(ImageRef::new(&source, 64, 64), Rect::new(0, 0, 64, 64))
            .unwrap()
            .to_vec();
        let mut milky = GlassSurface::new(light);
        let milky_pixels = milky
            .refresh(ImageRef::new(&source, 64, 64), Rect::new(0, 0, 64, 64))
            .unwrap()
            .to_vec();

        assert!(plain_pixels[0] < 8);
        assert!(milky_pixels[0] > 240);
    }

    #[test]
    fn render_image_is_bgra_and_invalidated_by_style_changes() {
        let source = gradient(32, 32);
        // Identity color stage so the pixels equal the source and the channel
        // order is observable.
        let passthrough = GlassStyle {
            blur: 0.0,
            saturation: 1.0,
            brightness: 1.0,
            contrast: 1.0,
            tint: Rgba8::TRANSPARENT,
            corner: 0.0,
            quality: Quality::Balanced,
        };
        let mut surface = GlassSurface::new(passthrough);
        surface
            .refresh(ImageRef::new(&source, 32, 32), Rect::new(0, 0, 32, 32))
            .unwrap();

        let image = surface.render_image().expect("image");
        assert_eq!(image.size(0).width.0, 32);
        let bytes = image.as_bytes(0).expect("frame bytes");
        // First source pixel is (0, 0, 200, 255): stored as BGRA -> red and blue
        // swapped, so byte 0 is the blue channel (200) and byte 2 the red (0).
        assert_eq!(bytes[0], 200, "blue channel first");
        assert_eq!(bytes[2], 0, "red channel last among the color bytes");

        surface.set_style(MENU);
        assert!(surface.render_image().is_none(), "style change invalidates");
        surface
            .refresh(ImageRef::new(&source, 32, 32), Rect::new(0, 0, 32, 32))
            .unwrap();
        assert!(surface.render_image().is_some());
    }

    #[test]
    fn presets_carry_the_design_tokens() {
        assert_eq!(DIALOG.params().corners, CornerRadius::all(crate::theme::radius::LG));
        assert_eq!(DIALOG.params().blur_radius, 24.0);
        assert!(DIALOG_DARK.params().brightness < 1.0);
        assert_eq!(MENU.params().quality, Quality::Fast);
    }

    #[test]
    fn builder_sets_the_style_refinement() {
        // The renderer side (`Scene`/blade) is verified by the
        // `backdrop_check` example; this covers the builder -> style hop.
        use gpui::prelude::*;
        let mut element = gpui::div().backdrop_filter(gpui::px(20.0), gpui::hsla(0.0, 0.0, 1.0, 0.3));
        let filter = element.style().backdrop_filter.expect("style field is set");
        assert_eq!(filter.blur_radius, 20.0);
        assert_eq!(filter.tint.a, 0.3);
        assert!(!filter.is_identity());

        let mut untouched = gpui::div();
        assert!(
            untouched.style().backdrop_filter.is_none(),
            "elements are transparent by default"
        );
    }
}
