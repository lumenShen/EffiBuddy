//! EffiBuddy design system built on GPUI primitives.
//!
//! Everything here is app-agnostic: tokens, basic controls, a multiline text
//! editor and a Markdown renderer. The application composes these pieces into
//! its shell.

pub mod assets;
pub mod button;
pub mod markdown;
pub mod primitives;
pub mod scroll;
pub mod table;
pub mod text_input;
pub mod theme;

pub use assets::{EffiBuddyAssets, icons};
pub use button::{Button, ButtonSize, ButtonVariant, IconButton};
pub use markdown::{
    chunk_ranges, markdown_cache_hits, markdown_inline, markdown_source_key, render_markdown,
    render_markdown_chunk, render_markdown_keyed, render_markdown_streaming,
};
pub use primitives::{
    BadgeTone, Spinner, TooltipView, avatar, badge, card, chip, divider, dot, elevated_card,
    empty_state, kbd, section_label, shadow_float, small_badge, switch, tooltip, vertical_divider,
};
pub use scroll::{
    GlideTick, SCROLL_GLIDE_CUTOFF, SCROLL_GLIDE_TAU, ScrollGlide, SmoothScrollHandle,
    WHEEL_LINE_HEIGHT, wheel_capture_overlay,
};
pub use text_input::{TextInputOptions, TextInputState, TextInputView, install_keybindings};
pub use theme::{Theme, ThemeMode, alpha_color, color, line, radius, space, text};
