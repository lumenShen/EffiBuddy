//! Accent presets for the soft/glass ("mist") surface language.
//!
//! The palette is a port of the `deepseek-harness` mobile client's accent
//! table: six low-chroma seeds, one container and one user-bubble tone each,
//! plus the dark-mode lift. Keeping the numbers here (rather than inline in the
//! app) means a new preset is one table row and every mobile component follows.
//!
//! Values are copied verbatim from the reference implementation, so a theme
//! switch cannot drift from the design it was approved against.

use gpui::Hsla;

use crate::theme::color;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AccentId {
    /// 雾蓝 — the default: a cool, low-chroma blue.
    #[default]
    Mist,
    /// 黛青 — teal-leaning slate blue.
    Daiqing,
    /// 暮紫 — dusk violet (also the "agent is planning" semantic tone).
    Dusk,
    /// 松石 — turquoise.
    Turquoise,
    /// 琥珀 — warm amber.
    Amber,
    /// 石墨 — graphite neutral.
    Graphite,
}

impl AccentId {
    /// Display order in the settings swatch row.
    pub const ALL: &'static [AccentId] = &[
        AccentId::Mist,
        AccentId::Daiqing,
        AccentId::Dusk,
        AccentId::Turquoise,
        AccentId::Amber,
        AccentId::Graphite,
    ];

    pub fn id(self) -> &'static str {
        match self {
            AccentId::Mist => "mist",
            AccentId::Daiqing => "daiqing",
            AccentId::Dusk => "dusk",
            AccentId::Turquoise => "turquoise",
            AccentId::Amber => "amber",
            AccentId::Graphite => "graphite",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AccentId::Mist => "雾蓝",
            AccentId::Daiqing => "黛青",
            AccentId::Dusk => "暮紫",
            AccentId::Turquoise => "松石",
            AccentId::Amber => "琥珀",
            AccentId::Graphite => "石墨",
        }
    }

    /// Parse a persisted id; unknown or missing values fall back to 雾蓝, which
    /// keeps old config files and forward-written ids from failing to load.
    pub fn from_id(value: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|accent| accent.id() == value)
            .unwrap_or(AccentId::Mist)
    }

    pub fn colors(self) -> AccentColors {
        match self {
            AccentId::Mist => AccentColors {
                seed: 0x4B67B4,
                pressed: 0x3D569D,
                container: 0xDEE8FF,
                on_container: 0x213262,
                bubble: 0xD9E4FD,
                glow: 0xABC3FC,
                seed_dark: 0x97B1F0,
                pressed_dark: 0x859EDC,
                on_seed_dark: 0x121C34,
                container_dark: 0x2E3C5F,
                on_container_dark: 0xD8E1F6,
                bubble_dark: 0x293144,
                glow_dark: 0x51679D,
                backdrop_top: 0xEBF1FE,
                backdrop_bottom: 0xF6F7FC,
                backdrop_top_dark: 0x1D222C,
                backdrop_bottom_dark: 0x1B1C23,
            },
            AccentId::Daiqing => AccentColors {
                seed: 0x236185,
                pressed: 0x145071,
                container: 0xDBEAF5,
                on_container: 0x123950,
                bubble: 0xD6E7F3,
                glow: 0xA2CAE5,
                seed_dark: 0x8CB9D7,
                pressed_dark: 0x79A6C4,
                on_seed_dark: 0x0D1F2B,
                container_dark: 0x274152,
                on_container_dark: 0xD5E3ED,
                bubble_dark: 0x26333C,
                glow_dark: 0x466E88,
                backdrop_top: 0xE7F0FA,
                backdrop_bottom: 0xF6F8FC,
                backdrop_top_dark: 0x1B242C,
                backdrop_bottom_dark: 0x1A1D22,
            },
            AccentId::Dusk => AccentColors {
                seed: 0x726291,
                pressed: 0x61517D,
                container: 0xEAE5F4,
                on_container: 0x392F4C,
                bubble: 0xE6E1F1,
                glow: 0xC8BDE1,
                seed_dark: 0xB8ABD2,
                pressed_dark: 0xA598BF,
                on_seed_dark: 0x1F1A29,
                container_dark: 0x40394F,
                on_container_dark: 0xE2DEEC,
                bubble_dark: 0x332F3B,
                glow_dark: 0x6D6284,
                backdrop_top: 0xEFEAFA,
                backdrop_bottom: 0xF8F6FD,
                backdrop_top_dark: 0x221E2B,
                backdrop_bottom_dark: 0x1C1B22,
            },
            AccentId::Turquoise => AccentColors {
                seed: 0x34766F,
                pressed: 0x296760,
                container: 0xDBECE9,
                on_container: 0x123D39,
                bubble: 0xD6E8E6,
                glow: 0xA3CEC8,
                seed_dark: 0x8DBEB7,
                pressed_dark: 0x7BABA4,
                on_seed_dark: 0x0D211F,
                container_dark: 0x274340,
                on_container_dark: 0xD6E5E2,
                bubble_dark: 0x263533,
                glow_dark: 0x47726D,
                backdrop_top: 0xE6F2F1,
                backdrop_bottom: 0xF6FAFA,
                backdrop_top_dark: 0x1A2624,
                backdrop_bottom_dark: 0x191F1F,
            },
            AccentId::Amber => AccentColors {
                seed: 0x8D6636,
                pressed: 0x7A5528,
                container: 0xF2E6D9,
                on_container: 0x483011,
                bubble: 0xEEE2D3,
                glow: 0xDCBE9D,
                seed_dark: 0xCCAC87,
                pressed_dark: 0xB99975,
                on_seed_dark: 0x271A0C,
                container_dark: 0x4C3A25,
                on_container_dark: 0xEADFD3,
                bubble_dark: 0x392F24,
                glow_dark: 0x7F6343,
                backdrop_top: 0xFAF1E7,
                backdrop_bottom: 0xFDF9F5,
                backdrop_top_dark: 0x262019,
                backdrop_bottom_dark: 0x201D19,
            },
            AccentId::Graphite => AccentColors {
                seed: 0x2C2E32,
                pressed: 0x1D1F23,
                container: 0xE3E6EC,
                on_container: 0x2C2E32,
                bubble: 0xE0E3E8,
                glow: 0xBBBEC3,
                seed_dark: 0xE5E8ED,
                pressed_dark: 0xCED1D6,
                on_seed_dark: 0x191B1E,
                container_dark: 0x3B3D41,
                on_container_dark: 0xDEE1E7,
                bubble_dark: 0x2E3034,
                glow_dark: 0x616368,
                backdrop_top: 0xEFF1F5,
                backdrop_bottom: 0xF8F9FB,
                backdrop_top_dark: 0x1E2024,
                backdrop_bottom_dark: 0x1B1C1F,
            },
        }
    }
}

/// One accent's role colors, as raw `0xRRGGBB` literals.
#[derive(Clone, Copy, Debug)]
pub struct AccentColors {
    pub seed: u32,
    pub pressed: u32,
    pub container: u32,
    pub on_container: u32,
    pub bubble: u32,
    pub glow: u32,
    pub seed_dark: u32,
    pub pressed_dark: u32,
    pub on_seed_dark: u32,
    pub container_dark: u32,
    pub on_container_dark: u32,
    pub bubble_dark: u32,
    pub glow_dark: u32,
    pub backdrop_top: u32,
    pub backdrop_bottom: u32,
    pub backdrop_top_dark: u32,
    pub backdrop_bottom_dark: u32,
}

impl AccentColors {
    pub fn seed(self) -> Hsla {
        color(self.seed)
    }

    pub fn pressed(self) -> Hsla {
        color(self.pressed)
    }

    pub fn container(self) -> Hsla {
        color(self.container)
    }

    pub fn on_container(self) -> Hsla {
        color(self.on_container)
    }

    pub fn bubble(self) -> Hsla {
        color(self.bubble)
    }

    pub fn glow(self) -> Hsla {
        color(self.glow)
    }

    pub fn seed_dark(self) -> Hsla {
        color(self.seed_dark)
    }

    pub fn pressed_dark(self) -> Hsla {
        color(self.pressed_dark)
    }

    pub fn on_seed_dark(self) -> Hsla {
        color(self.on_seed_dark)
    }

    pub fn container_dark(self) -> Hsla {
        color(self.container_dark)
    }

    pub fn on_container_dark(self) -> Hsla {
        color(self.on_container_dark)
    }

    pub fn bubble_dark(self) -> Hsla {
        color(self.bubble_dark)
    }

    pub fn glow_dark(self) -> Hsla {
        color(self.glow_dark)
    }

    pub fn backdrop_top(self) -> Hsla {
        color(self.backdrop_top)
    }

    pub fn backdrop_bottom(self) -> Hsla {
        color(self.backdrop_bottom)
    }

    pub fn backdrop_top_dark(self) -> Hsla {
        color(self.backdrop_top_dark)
    }

    pub fn backdrop_bottom_dark(self) -> Hsla {
        color(self.backdrop_bottom_dark)
    }

    /// The seed (or its dark lift) for the active mode. Useful for icon tints
    /// that must not care which brightness is on screen.
    pub fn seed_for(self, dark: bool) -> Hsla {
        if dark { self.seed_dark() } else { self.seed() }
    }

    /// Contrast-safe text on top of the seed.
    pub fn on_seed_for(self, dark: bool) -> Hsla {
        if dark {
            self.on_seed_dark()
        } else {
            crate::theme::color(0xFFFFFF)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_accent_ids_fall_back_to_mist() {
        assert_eq!(AccentId::from_id("mist"), AccentId::Mist);
        assert_eq!(AccentId::from_id("graphite"), AccentId::Graphite);
        assert_eq!(AccentId::from_id(""), AccentId::Mist);
        assert_eq!(AccentId::from_id("nope"), AccentId::Mist);
    }

    #[test]
    fn every_preset_roundtrips_through_its_id_and_has_a_label() {
        for accent in AccentId::ALL {
            assert_eq!(AccentId::from_id(accent.id()), *accent);
            assert!(!accent.label().is_empty());
        }
    }

    #[test]
    fn dark_seeds_are_lighter_than_light_seeds() {
        // The reference lifts dark-mode accents so they read on a slate page.
        for accent in AccentId::ALL {
            let colors = accent.colors();
            let light = rgb_luma(colors.seed);
            let dark = rgb_luma(colors.seed_dark);
            if *accent != AccentId::Graphite {
                assert!(dark > light, "{} dark seed should be lighter", accent.id());
            }
        }
    }

    fn rgb_luma(value: u32) -> f32 {
        0.2126 * ((value >> 16) & 0xFF) as f32
            + 0.7152 * ((value >> 8) & 0xFF) as f32
            + 0.0722 * (value & 0xFF) as f32
    }
}
