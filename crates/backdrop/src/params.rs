//! User-facing knobs for the backdrop filter.
//!
//! These mirror the useful subset of CSS `backdrop-filter`:
//! `blur() saturate() brightness() contrast()` plus a tint color and the
//! rounded corners of the panel the backdrop is seen through.

/// An 8-bit straight-alpha RGBA color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Build from `0xRRGGBB` plus a separate alpha, matching `effibuddy_ui::color`.
    pub const fn from_rgb_alpha(rgb: u32, a: u8) -> Self {
        Self {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a,
        }
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }
}

/// Per-corner radius of the panel, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct CornerRadius {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl CornerRadius {
    pub const ZERO: Self = Self {
        top_left: 0.0,
        top_right: 0.0,
        bottom_right: 0.0,
        bottom_left: 0.0,
    };

    pub const fn all(radius: f32) -> Self {
        Self {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        }
    }

    pub fn max(&self) -> f32 {
        self.top_left
            .max(self.top_right)
            .max(self.bottom_right)
            .max(self.bottom_left)
    }

    pub fn is_zero(&self) -> bool {
        self.max() <= 0.0
    }
}

/// How much quality the pipeline is allowed to trade for speed.
///
/// The dominant cost of a wide blur is the blur itself, so the pipeline always
/// resolves it at a reduced resolution: a blur of `sigma` pixels looks the same
/// when it is computed on a 1/n downsample with `sigma / n`. `Quality` bounds
/// how large the resulting low-resolution sigma may be, which keeps the
/// convolution taps in single digits regardless of the CSS radius.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    /// Widest downsampling, smallest kernels. Good for overlays that move.
    Fast,
    /// Default: visually identical to `High` for glass panels.
    #[default]
    Balanced,
    /// Smaller downsampling, larger kernels; only worth it for still frames.
    High,
}

impl Quality {
    /// Largest sigma (in low-resolution pixels) a backend may work with.
    pub const fn max_low_res_sigma(self) -> f32 {
        match self {
            Self::Fast => 1.5,
            Self::Balanced => 2.5,
            Self::High => 4.0,
        }
    }

}

/// Everything the filter needs besides the source pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackdropParams {
    /// CSS-equivalent blur radius in physical pixels. `0` disables the blur.
    pub blur_radius: f32,
    /// `1.0` leaves colors untouched.
    pub saturation: f32,
    /// `1.0` leaves colors untouched.
    pub brightness: f32,
    /// `1.0` leaves colors untouched.
    pub contrast: f32,
    /// Color composited on top of the blurred backdrop (straight alpha).
    pub tint: Rgba8,
    /// Rounded corners of the panel, in physical pixels.
    pub corners: CornerRadius,
    /// Width of the anti-aliased edge on the rounded corners, in pixels.
    pub edge_softness: f32,
    pub quality: Quality,
}

impl Default for BackdropParams {
    fn default() -> Self {
        Self {
            blur_radius: 24.0,
            saturation: 1.0,
            brightness: 1.0,
            contrast: 1.0,
            tint: Rgba8::TRANSPARENT,
            corners: CornerRadius::ZERO,
            edge_softness: 1.0,
            quality: Quality::Balanced,
        }
    }
}

impl BackdropParams {
    pub fn is_blurred(&self) -> bool {
        self.blur_radius > 0.5
    }

    /// True when the color/tint stage is an identity transform.
    pub fn is_color_identity(&self) -> bool {
        (self.saturation - 1.0).abs() < 1e-4
            && (self.brightness - 1.0).abs() < 1e-4
            && (self.contrast - 1.0).abs() < 1e-4
            && self.tint.is_transparent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_params_are_identity_colors() {
        let params = BackdropParams::default();
        assert!(params.is_color_identity());
        assert!(params.is_blurred());
    }

    #[test]
    fn tint_breaks_color_identity() {
        let params = BackdropParams {
            tint: Rgba8::opaque(255, 255, 255),
            ..BackdropParams::default()
        };
        assert!(!params.is_color_identity());
    }
}
