//! Design tokens: neutral paper.
//!
//! The palette is deliberately low-chroma: white surfaces, neutral gray
//! scaffolding, hairline ink borders, and near-black primary actions. Color is
//! reserved for semantic status. Components take `&Theme` explicitly so
//! rendering never depends on hidden globals.

use gpui::{App, Global, Hsla, rgb, rgba as gpui_rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: ThemeMode,

    pub bg: Hsla,
    pub sidebar: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub elevated: Hsla,
    pub hover: Hsla,
    pub active: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,

    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,

    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_soft: Hsla,
    pub on_accent: Hsla,

    pub danger: Hsla,
    pub danger_hover: Hsla,
    pub danger_soft: Hsla,
    pub success: Hsla,
    pub success_soft: Hsla,
    pub warning: Hsla,
    pub warning_soft: Hsla,

    pub selection: Hsla,
    pub code_bg: Hsla,
    /// Markdown link color. ZCode renders links in a single sky blue that
    /// reads as clickable without competing with status colors.
    pub link: Hsla,
    pub user_bubble: Hsla,
    pub tool_bg: Hsla,
    pub shadow: Hsla,

    /// Stable accents for subagent profiles. Blue/green/purple/amber echo
    /// ZCode's subagent identity colors without changing the neutral base.
    pub agent_blue: Hsla,
    pub agent_green: Hsla,
    pub agent_purple: Hsla,
    pub agent_amber: Hsla,
    /// Dimming layer for modal overlays.
    pub overlay: Hsla,

    /// Keyboard/input focus ring. Soft enough to sit on any surface.
    pub focus_ring: Hsla,
    pub disabled_bg: Hsla,
    pub disabled_fg: Hsla,
    pub scrollbar: Hsla,
    pub scrollbar_hover: Hsla,
    pub tooltip_bg: Hsla,
    pub tooltip_fg: Hsla,
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

impl Theme {
    /// Neutral light. White cards on a barely-gray page, ink-black actions.
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            bg: color(0xFAFAFA),
            sidebar: color(0xF4F4F5),
            surface: color(0xFFFFFF),
            surface_alt: color(0xF2F2F3),
            elevated: color(0xFFFFFF),
            hover: alpha_color(0x18181B0D),
            active: alpha_color(0x18181B14),
            border: alpha_color(0x18181B14),
            border_strong: alpha_color(0x18181B29),
            text: color(0x18181B),
            text_muted: color(0x52525B),
            text_faint: color(0x71717A),
            accent: color(0x18181B),
            accent_hover: color(0x3F3F46),
            accent_soft: color(0xE8E8EB),
            on_accent: color(0xFFFFFF),
            danger: color(0xDC2626),
            danger_hover: color(0xB91C1C),
            danger_soft: color(0xFEF2F2),
            success: color(0x15803D),
            success_soft: color(0xF0FDF4),
            warning: color(0xB45309),
            warning_soft: color(0xFFFBEB),
            selection: alpha_color(0x18181B2E),
            code_bg: color(0xF4F4F5),
            link: color(0x0EA5E9),
            user_bubble: color(0xF4F4F5),
            tool_bg: color(0xFFFFFF),
            shadow: alpha_color(0x0000001A),
            agent_blue: color(0x2563EB),
            agent_green: color(0x15803D),
            agent_purple: color(0x7C3AED),
            agent_amber: color(0xB45309),
            overlay: alpha_color(0x00000066),
            focus_ring: alpha_color(0x18181B33),
            disabled_bg: color(0xEFEFF1),
            disabled_fg: color(0xA1A1AA),
            scrollbar: alpha_color(0x18181B2E),
            scrollbar_hover: alpha_color(0x18181B52),
            tooltip_bg: color(0x18181B),
            tooltip_fg: color(0xFAFAFA),
        }
    }

    /// Neutral charcoal. Primary actions invert to near-white.
    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            bg: color(0x121212),
            sidebar: color(0x171717),
            surface: color(0x1C1C1C),
            surface_alt: color(0x242424),
            elevated: color(0x242424),
            hover: alpha_color(0xFFFFFF0F),
            active: alpha_color(0xFFFFFF1A),
            border: alpha_color(0xFFFFFF14),
            border_strong: alpha_color(0xFFFFFF29),
            text: color(0xEDEDED),
            text_muted: color(0xA3A3A3),
            text_faint: color(0x8A8A8A),
            accent: color(0xF5F5F5),
            accent_hover: color(0xFFFFFF),
            accent_soft: color(0x2A2A2D),
            on_accent: color(0x141414),
            danger: color(0xF87171),
            danger_hover: color(0xFCA5A5),
            danger_soft: color(0x2D1A1A),
            success: color(0x6CC79A),
            success_soft: color(0x14281E),
            warning: color(0xE0A45C),
            warning_soft: color(0x2B2216),
            selection: alpha_color(0xFFFFFF38),
            code_bg: color(0x0E0E0E),
            link: color(0x0284C7),
            user_bubble: color(0x262626),
            tool_bg: color(0x1C1C1C),
            shadow: alpha_color(0x000000B3),
            agent_blue: color(0x7EA6F0),
            agent_green: color(0x6CC79A),
            agent_purple: color(0xC4A0F0),
            agent_amber: color(0xE0A45C),
            overlay: alpha_color(0x00000099),
            focus_ring: alpha_color(0xFFFFFF3D),
            disabled_bg: color(0x262626),
            disabled_fg: color(0x6E6E73),
            scrollbar: alpha_color(0xFFFFFF29),
            scrollbar_hover: alpha_color(0xFFFFFF4D),
            tooltip_bg: color(0xEDEDED),
            tooltip_fg: color(0x121212),
        }
    }

    pub fn toggle(&self) -> Self {
        match self.mode {
            ThemeMode::Dark => Self::light(),
            ThemeMode::Light => Self::dark(),
        }
    }

    pub fn is_dark(&self) -> bool {
        self.mode.is_dark()
    }

    pub fn ui_font(&self) -> &'static str {
        "Segoe UI"
    }

    pub fn mono_font(&self) -> &'static str {
        "Consolas"
    }

    pub fn set_global(self, cx: &mut App) {
        cx.set_global(self);
    }

    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }
}

impl Global for Theme {}

pub fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn alpha_color(hex: u32) -> Hsla {
    gpui_rgba(hex).into()
}

pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

pub mod radius {
    pub const XS: f32 = 6.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 10.0;
    pub const LG: f32 = 14.0;
    pub const XL: f32 = 18.0;
    pub const XXL: f32 = 22.0;
    pub const FULL: f32 = 999.0;
}

pub mod text {
    pub const MICRO: f32 = 10.0;
    pub const XS: f32 = 11.0;
    pub const SM: f32 = 12.0;
    pub const MD: f32 = 13.0;
    pub const BASE: f32 = 14.0;
    pub const LG: f32 = 15.0;
    pub const TITLE: f32 = 17.0;
    pub const XL: f32 = 22.0;
}

pub mod line {
    pub const TIGHT: f32 = 16.0;
    pub const BASE: f32 = 20.0;
    pub const RELAXED: f32 = 23.0;
    pub const CODE: f32 = 18.0;
    pub const DISPLAY: f32 = 30.0;
}
