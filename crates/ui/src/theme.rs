//! Design tokens: neutral paper.
//!
//! The palette is deliberately low-chroma: white surfaces, neutral gray
//! scaffolding, hairline ink borders, and near-black primary actions. Color is
//! reserved for semantic status. Components take `&Theme` explicitly so
//! rendering never depends on hidden globals.

use gpui::{App, Global, Hsla, rgb, rgba as gpui_rgba};

use crate::accent::AccentId;

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

    // --- soft/glass layer -------------------------------------------------
    //
    // The desktop shell only needs flat surfaces, but the mobile shell is a
    // "mist glass" design: a tinted vertical gradient page, frosted chrome and
    // cards, and an accent-tinted user bubble. Those roles live here so the
    // mobile views never hard-code a color and an accent switch is one field
    // assignment. On the desktop presets the values degrade to the flat
    // palette above.
    /// Page gradient, top stop.
    pub backdrop_top: Hsla,
    /// Page gradient, bottom stop.
    pub backdrop_bottom: Hsla,
    /// Accent glow blob painted behind the content.
    pub glow_primary: Hsla,
    /// Secondary (haze) glow blob.
    pub glow_secondary: Hsla,
    /// Frosted chrome fill (top bar, composer, pills).
    pub glass: Hsla,
    /// Frosted sheet fill (drawer, dialogs).
    pub glass_strong: Hsla,
    /// Hairline drawn on frosted surfaces.
    pub glass_border: Hsla,
    /// Inner highlight along the top edge of a frosted surface.
    pub glass_rim: Hsla,
    /// Text on the accent-tinted user bubble.
    pub bubble_text: Hsla,
    /// Accent-tinted user bubble fill.
    pub accent_bubble: Hsla,
    /// Accent glow used for rings/selection.
    pub accent_glow: Hsla,
    /// Tertiary semantic tone ("the agent is planning"): violet dusk.
    pub tertiary: Hsla,
    pub tertiary_soft: Hsla,
    /// Backdrop behind a modal/drawer.
    pub scrim: Hsla,
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
            backdrop_top: color(0xFAFAFA),
            backdrop_bottom: color(0xFFFFFF),
            glow_primary: alpha_color(0x00000000),
            glow_secondary: alpha_color(0x00000000),
            glass: color(0xFFFFFF),
            glass_strong: color(0xFFFFFF),
            glass_border: alpha_color(0x18181B14),
            glass_rim: alpha_color(0xFFFFFF00),
            bubble_text: color(0x18181B),
            accent_bubble: color(0xF4F4F5),
            accent_glow: color(0xE8E8EB),
            tertiary: color(0x7C3AED),
            tertiary_soft: color(0xF1EBFE),
            scrim: alpha_color(0x00000066),
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
            backdrop_top: color(0x121212),
            backdrop_bottom: color(0x0E0E0E),
            glow_primary: alpha_color(0x00000000),
            glow_secondary: alpha_color(0x00000000),
            glass: color(0x1C1C1C),
            glass_strong: color(0x242424),
            glass_border: alpha_color(0xFFFFFF14),
            glass_rim: alpha_color(0xFFFFFF00),
            bubble_text: color(0xEDEDED),
            accent_bubble: color(0x262626),
            accent_glow: color(0x2A2A2D),
            tertiary: color(0xC4A0F0),
            tertiary_soft: color(0x2B2338),
            scrim: alpha_color(0x00000099),
        }
    }

    /// Soft/glass light theme: a mist-blue page gradient, frosted white chrome
    /// and the accent's bubble tone. This is the mobile shell's surface
    /// language; the desktop presets keep the flat neutrals above.
    pub fn mobile_light(accent: AccentId) -> Self {
        let a = accent.colors();
        let mut theme = Self::light();
        theme.bg = color(0xFAFAFA);
        theme.sidebar = color(0xF7F7F8);
        theme.surface = color(0xFFFFFF);
        theme.surface_alt = color(0xF2F2F3);
        theme.elevated = color(0xFFFFFF);
        theme.hover = alpha_color(0x1E232E0F);
        theme.active = alpha_color(0x1E232E1A);
        theme.border = alpha_color(0x1E232E12);
        theme.border_strong = color(0xC9CED8);
        theme.text = color(0x1E232E);
        theme.text_muted = color(0x4F5562);
        theme.text_faint = color(0x646974);
        theme.accent = a.seed();
        theme.accent_hover = a.pressed();
        theme.accent_soft = a.container();
        theme.on_accent = color(0xFFFFFF);
        theme.danger = color(0xA54743);
        theme.danger_hover = color(0x8C3A36);
        theme.danger_soft = color(0xFFE2E0);
        theme.success = color(0x2F7A5B);
        theme.success_soft = color(0xE3F2EA);
        theme.warning = color(0x8A6320);
        theme.warning_soft = color(0xF7ECD9);
        theme.selection = alpha_color(0x1E232E26);
        theme.code_bg = color(0xF6F9FE);
        theme.link = a.seed();
        theme.user_bubble = a.bubble();
        theme.tool_bg = color(0xFBFCFF);
        theme.shadow = alpha_color(0x0B152D1F);
        theme.agent_blue = color(0x4B67B4);
        theme.agent_green = color(0x34766F);
        theme.agent_purple = color(0x726291);
        theme.agent_amber = color(0x8D6636);
        theme.overlay = alpha_color(0x0B111F52);
        theme.scrim = alpha_color(0x0B111F52);
        theme.focus_ring = alpha_color(0x7A8FD62E);
        theme.disabled_bg = color(0xE7EBF4);
        theme.disabled_fg = color(0xA1A5AC);
        theme.scrollbar = alpha_color(0x1E232E26);
        theme.scrollbar_hover = alpha_color(0x1E232E45);
        theme.tooltip_bg = color(0x1E232E);
        theme.tooltip_fg = color(0xF6F7FC);
        // Neutral first: the mobile shell shares the desktop's gray/white/ink
        // base. The accent stays a 5% element (primary buttons, send key,
        // selection, links) instead of tinting whole surfaces.
        theme.backdrop_top = color(0xFAFAFA);
        theme.backdrop_bottom = color(0xFFFFFF);
        theme.glow_primary = alpha_color(0x00000000);
        theme.glow_secondary = alpha_color(0x00000000);
        theme.glass = color(0xFFFFFF).opacity(0.94);
        theme.glass_strong = color(0xFFFFFF).opacity(0.98);
        theme.glass_border = alpha_color(0x18181B14);
        theme.glass_rim = color(0xFFFFFF).opacity(0.85);
        theme.bubble_text = color(0x18181B);
        theme.accent_bubble = color(0xF2F2F3);
        theme.accent_glow = a.glow();
        theme.tertiary = color(0x6B578F);
        theme.tertiary_soft = color(0xECE5FB);
        theme
    }

    /// Soft/glass dark theme: deep-sea slate, smoke glass and the accent's
    /// lifted seed (light text is never pure white).
    pub fn mobile_dark(accent: AccentId) -> Self {
        let a = accent.colors();
        let mut theme = Self::dark();
        theme.bg = color(0x121212);
        theme.sidebar = color(0x171717);
        theme.surface = color(0x1C1C1C);
        theme.surface_alt = color(0x242424);
        theme.elevated = color(0x2D323C);
        theme.hover = alpha_color(0xFFFFFF12);
        theme.active = alpha_color(0xFFFFFF1F);
        theme.border = alpha_color(0xFFFFFF14);
        theme.border_strong = color(0x3C414C);
        theme.text = color(0xE6E9F0);
        theme.text_muted = color(0xB6BBC5);
        theme.text_faint = color(0x9A9EA9);
        theme.accent = a.seed_dark();
        theme.accent_hover = a.pressed_dark();
        theme.accent_soft = a.container_dark();
        theme.on_accent = a.on_seed_dark();
        theme.danger = color(0xE69089);
        theme.danger_hover = color(0xF0AEA8);
        theme.danger_soft = color(0x542C29);
        theme.success = color(0x8DD3B2);
        theme.success_soft = color(0x23372E);
        theme.warning = color(0xD7B486);
        theme.warning_soft = color(0x3A2F1F);
        theme.selection = alpha_color(0xFFFFFF30);
        theme.code_bg = color(0x14171E);
        theme.link = a.seed_dark();
        theme.user_bubble = a.bubble_dark();
        theme.tool_bg = color(0x232730);
        theme.shadow = alpha_color(0x03060D59);
        theme.agent_blue = color(0x97B1F0);
        theme.agent_green = color(0x8DBEB7);
        theme.agent_purple = color(0xB8ABD2);
        theme.agent_amber = color(0xCCAC87);
        theme.overlay = alpha_color(0x0000007A);
        theme.scrim = alpha_color(0x0000007A);
        theme.focus_ring = alpha_color(0x97B1F03D);
        theme.disabled_bg = color(0x2A2E37);
        theme.disabled_fg = color(0x656971);
        theme.scrollbar = alpha_color(0xFFFFFF26);
        theme.scrollbar_hover = alpha_color(0xFFFFFF45);
        theme.tooltip_bg = color(0xE6E9F0);
        theme.tooltip_fg = color(0x1C1F28);
        theme.backdrop_top = color(0x121212);
        theme.backdrop_bottom = color(0x0E0E0E);
        theme.glow_primary = alpha_color(0x00000000);
        theme.glow_secondary = alpha_color(0x00000000);
        theme.glass = color(0x1F1F1F).opacity(0.94);
        theme.glass_strong = color(0x171717).opacity(0.98);
        theme.glass_border = alpha_color(0xFFFFFF14);
        theme.glass_rim = alpha_color(0xFFFFFF17);
        theme.bubble_text = color(0xEDEDED);
        theme.accent_bubble = color(0x262626);
        theme.accent_glow = a.glow_dark();
        theme.tertiary = color(0xBEAEDE);
        theme.tertiary_soft = color(0x3E3451);
        theme
    }

    /// Rebuild the current brightness with the soft/glass surface language.
    pub fn mobile(self: &Theme, accent: AccentId) -> Theme {
        if self.is_dark() {
            Theme::mobile_dark(accent)
        } else {
            Theme::mobile_light(accent)
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

/// Mobile shell geometry and type scale.
///
/// The numbers live in `effibuddy-mobile` so the desktop renderer and the
/// planned on-device renderer cannot drift; this module is a thin re-export
/// under the name the UI code already uses.
pub mod mobile {
    pub use effibuddy_mobile::layout::*;
}

