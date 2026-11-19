//! Phone geometry and type scale.
//!
//! One source of truth for both shells. The GPUI renderer re-exports these
//! numbers as `effibuddy_ui::mobile`, so the crate that draws and the crate
//! that describes agree by construction.
//!
//! Values follow the reference mobile client's shape table: 16px gutters,
//! 20px bubbles, 14px cards, 28px sheets, a 52px top bar and 44px minimum
//! touch targets.

/// Page gutter on the left/right edge.
pub const GUTTER: f32 = 16.0;
/// Width of the phone column when the window is wider than a phone.
pub const PHONE_WIDTH: f32 = 420.0;
/// Below this width the column fills the window instead of floating.
pub const FULL_BLEED_BELOW: f32 = 470.0;

pub const BAR_HEIGHT: f32 = 52.0;
pub const COMPOSER_MIN_HEIGHT: f32 = 52.0;
pub const DRAWER_WIDTH: f32 = 312.0;
pub const LIST_ROW_HEIGHT: f32 = 44.0;
/// Minimum touch target.
pub const TOUCH: f32 = 44.0;

pub const RADIUS_CARD: f32 = 14.0;
pub const RADIUS_BUBBLE: f32 = 20.0;
pub const RADIUS_SHEET: f32 = 28.0;
pub const RADIUS_CHIP: f32 = 999.0;

pub const GAP_IN_TURN: f32 = 8.0;
pub const GAP_BETWEEN_TURNS: f32 = 24.0;

pub const TEXT_BODY: f32 = 15.5;
pub const LINE_BODY: f32 = 25.0;
pub const TEXT_META: f32 = 11.5;
pub const TEXT_TITLE: f32 = 17.0;
pub const TEXT_HEADLINE: f32 = 24.0;

/// Safe-area / soft-keyboard insets in logical px.
///
/// The desktop shell always passes [`Insets::ZERO`]; the on-device build fills
/// them from `WindowInsets` (display cutout + navigation gesture bar + IME).
/// Keeping this in the shared crate means neither renderer hard-codes a magic
/// bottom padding for the composer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f32,
    pub bottom: f32,
}

impl Insets {
    pub const ZERO: Insets = Insets {
        top: 0.0,
        bottom: 0.0,
    };

    pub const fn new(top: f32, bottom: f32) -> Self {
        Self { top, bottom }
    }

    pub fn is_zero(&self) -> bool {
        self.top == 0.0 && self.bottom == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insets_default_to_zero_and_detect_that() {
        assert!(Insets::ZERO.is_zero());
        assert!(Insets::default().is_zero());
        assert!(!Insets::new(12.0, 0.0).is_zero());
    }

    #[test]
    fn phone_geometry_matches_the_reference_shape_table() {
        assert_eq!(GUTTER, 16.0);
        assert_eq!(RADIUS_CARD, 14.0);
        assert_eq!(RADIUS_BUBBLE, 20.0);
        assert_eq!(RADIUS_SHEET, 28.0);
        assert!(TOUCH >= 44.0, "touch targets must stay thumb-sized");
    }
}
