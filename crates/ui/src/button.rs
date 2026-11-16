//! Buttons.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, ElementId, FontWeight, Hsla, IntoElement, RenderOnce, SharedString, Window,
    div, prelude::*, px, svg, transparent_black,
};

use crate::primitives::{Spinner, tooltip};
use crate::theme::{Theme, radius};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Danger,
    Soft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonSize {
    Sm,
    Md,
    Lg,
}

impl ButtonSize {
    fn height(self) -> f32 {
        match self {
            ButtonSize::Sm => 30.0,
            ButtonSize::Md => 34.0,
            ButtonSize::Lg => 38.0,
        }
    }

    fn padding_x(self) -> f32 {
        match self {
            ButtonSize::Sm => 12.0,
            ButtonSize::Md => 14.0,
            ButtonSize::Lg => 18.0,
        }
    }

    fn font_size(self) -> f32 {
        match self {
            ButtonSize::Sm => 12.0,
            ButtonSize::Md => 13.0,
            ButtonSize::Lg => 14.0,
        }
    }

    fn icon_size(self) -> f32 {
        match self {
            ButtonSize::Sm => 13.0,
            ButtonSize::Md => 15.0,
            ButtonSize::Lg => 17.0,
        }
    }
}

#[derive(Clone, Copy)]
struct ButtonColors {
    bg: Hsla,
    hover_bg: Hsla,
    fg: Hsla,
    border: Hsla,
}

fn button_colors(theme: &Theme, variant: ButtonVariant) -> ButtonColors {
    match variant {
        ButtonVariant::Primary => ButtonColors {
            bg: theme.accent,
            hover_bg: theme.accent_hover,
            fg: theme.on_accent,
            border: theme.accent,
        },
        ButtonVariant::Secondary => ButtonColors {
            bg: theme.surface,
            hover_bg: theme.surface_alt,
            fg: theme.text,
            border: theme.border,
        },
        ButtonVariant::Ghost => ButtonColors {
            bg: transparent_black(),
            hover_bg: theme.hover,
            fg: theme.text_muted,
            border: transparent_black(),
        },
        ButtonVariant::Danger => ButtonColors {
            bg: theme.danger,
            hover_bg: theme.danger_hover,
            fg: theme.on_accent,
            border: theme.danger,
        },
        ButtonVariant::Soft => ButtonColors {
            bg: theme.surface_alt,
            hover_bg: theme.active,
            fg: theme.text,
            border: transparent_black(),
        },
    }
}

#[derive(IntoElement)]
pub struct Button {
    id: SharedString,
    label: SharedString,
    icon: Option<&'static str>,
    variant: ButtonVariant,
    size: ButtonSize,
    disabled: bool,
    loading: bool,
    theme: Theme,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl Button {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>, theme: &Theme) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            variant: ButtonVariant::Secondary,
            size: ButtonSize::Md,
            disabled: false,
            loading: false,
            theme: theme.clone(),
            on_click: None,
        }
    }

    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = button_colors(&self.theme, self.variant);
        let (bg, fg, border) = if self.disabled {
            match self.variant {
                ButtonVariant::Primary | ButtonVariant::Danger | ButtonVariant::Soft => (
                    self.theme.disabled_bg,
                    self.theme.disabled_fg,
                    transparent_black(),
                ),
                ButtonVariant::Secondary => (
                    self.theme.surface,
                    self.theme.disabled_fg,
                    self.theme.border,
                ),
                ButtonVariant::Ghost => (
                    transparent_black(),
                    self.theme.disabled_fg,
                    transparent_black(),
                ),
            }
        } else {
            (colors.bg, colors.fg, colors.border)
        };
        let icon_size = self.size.icon_size();
        let mut element = div()
            .id(ElementId::Name(self.id.clone()))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(self.size.height()))
            .px(px(self.size.padding_x()))
            // Buttons have a fixed height, so a wrapped label would spill out
            // of the pill. Keep labels on one line; the button sizes itself to
            // the text instead of breaking it.
            .whitespace_nowrap()
            .rounded(px(radius::MD))
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(px(self.size.font_size()))
            .font_weight(FontWeight::MEDIUM);

        element = if self.disabled {
            element.opacity(0.62).cursor_default()
        } else {
            element
                .cursor_pointer()
                .hover(move |style| style.bg(colors.hover_bg))
                .active(|style| style.opacity(0.88))
        };

        if !self.disabled {
            if let Some(handler) = self.on_click.clone() {
                element = element.on_click(move |event, window, cx| handler(event, window, cx));
            }
        }

        if self.loading {
            element = element.child(Spinner::new(fg, icon_size));
        } else if let Some(icon) = self.icon {
            element = element.child(
                svg()
                    .path(icon)
                    .size(px(icon_size))
                    .text_color(fg)
                    .flex_none(),
            );
        }

        element.child(self.label)
    }
}

#[derive(IntoElement)]
pub struct IconButton {
    id: SharedString,
    icon: &'static str,
    label: SharedString,
    variant: ButtonVariant,
    size: f32,
    icon_size: f32,
    circle: bool,
    disabled: bool,
    theme: Theme,
    tooltip: Option<SharedString>,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl IconButton {
    pub fn new(id: impl Into<SharedString>, icon: &'static str, theme: &Theme) -> Self {
        Self {
            id: id.into(),
            icon,
            label: "".into(),
            variant: ButtonVariant::Ghost,
            size: 28.0,
            icon_size: 14.0,
            circle: false,
            disabled: false,
            theme: theme.clone(),
            tooltip: None,
            on_click: None,
        }
    }

    pub fn tooltip(mut self, label: impl Into<SharedString>) -> Self {
        self.tooltip = Some(label.into());
        self
    }

    pub fn circle(mut self, circle: bool) -> Self {
        self.circle = circle;
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: f32, icon_size: f32) -> Self {
        self.size = size;
        self.icon_size = icon_size;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = button_colors(&self.theme, self.variant);
        let mut element = div()
            .id(ElementId::Name(self.id.clone()))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(self.size))
            .rounded(px(if self.circle {
                radius::FULL
            } else {
                radius::SM
            }))
            .bg(colors.bg)
            .text_color(colors.fg);

        element = if self.disabled {
            element.opacity(0.35).cursor_default()
        } else {
            element
                .cursor_pointer()
                .hover(move |style| style.bg(colors.hover_bg))
                .active(|style| style.opacity(0.85))
        };

        element = element.child(
            svg()
                .path(self.icon)
                .size(px(self.icon_size))
                .text_color(colors.fg),
        );

        if !self.disabled {
            if let Some(handler) = self.on_click.clone() {
                element = element.on_click(move |event, window, cx| handler(event, window, cx));
            }
        }
        if let Some(label) = &self.tooltip {
            element = element.tooltip(tooltip(&self.theme, label.clone()));
        }
        if !self.label.is_empty() {
            element = element.child(self.label);
        }
        element
    }
}

pub fn icon_only(id: &'static str, icon: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.0))
        .rounded(px(radius::SM))
        .text_color(theme.text_muted)
        .hover(|style| style.bg(theme.hover).text_color(theme.text))
        .cursor_pointer()
        .child(svg().path(icon).size(px(15.0)).text_color(theme.text_muted))
}
