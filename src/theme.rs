//! Colors, fonts, type scale, and small shared widgets.

use std::{borrow::Cow, sync::Arc};

use gpui::{
    App, Div, ElementId, FontFeatures, FontWeight, Hsla, Rgba, SharedString, Stateful, div,
    prelude::*, px, rgb,
};

pub const BG: u32 = 0x0b0d11;
pub const HOVER: u32 = 0x161a21;
pub const SELECTED: u32 = 0x1b2029;
pub const BORDER: u32 = 0x1c2129;
pub const TEXT: u32 = 0xd6dbe3;
pub const MUTED: u32 = 0x5f6877;
pub const ACCENT: u32 = 0x4fd8e0;
pub const GREEN: u32 = 0x34d399;
pub const RED: u32 = 0xfb6f84;
pub const WARNING: u32 = 0xf5c451;

/// Monospace for data, tickers, and labels. Bundled; see `assets/fonts`.
pub const MONO: &str = "Lilex";
/// Proportional, for prose such as Claude's replies.
pub const SANS: &str = "IBM Plex Sans";

pub const TEXT_XS: f32 = 10.;
pub const TEXT_SM: f32 = 11.;
pub const TEXT_BASE: f32 = 12.;
pub const TEXT_LG: f32 = 15.;

/// Register the bundled fonts with gpui. Call once at startup.
pub fn load_fonts(cx: &App) -> anyhow::Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/lilex/Lilex-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/lilex/Lilex-Bold.ttf")),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"
        )),
    ])
}

/// Tabular figures, so columns of prices line up.
pub fn tabular_numbers() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// `color` at `alpha` opacity.
pub fn tint(color: u32, alpha: f32) -> Hsla {
    Hsla::from(rgb(color)).alpha(alpha)
}

/// A small uppercase section heading.
pub fn label(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(TEXT_XS))
        .text_color(rgb(MUTED))
        .child(text.into().to_uppercase())
}

/// A tinted button: `color` text on a faint `color` fill with a hairline
/// border, brightening on hover.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    color: u32,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2p5()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(tint(color, 0.35))
        .bg(tint(color, 0.10))
        .text_color(rgb(color))
        .text_size(px(TEXT_SM))
        .font_weight(FontWeight::BOLD)
        .cursor_pointer()
        .hover(move |style| style.bg(tint(color, 0.20)).border_color(tint(color, 0.6)))
        .child(label.into().to_uppercase())
}

/// A low-emphasis button, highlighted when `active`.
pub fn ghost_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_sm()
        .text_size(px(TEXT_SM))
        .cursor_pointer()
        .when(active, |b| b.bg(tint(ACCENT, 0.12)).text_color(rgb(ACCENT)))
        .when(!active, |b| {
            b.text_color(rgb(MUTED))
                .hover(|style| style.bg(rgb(HOVER)).text_color(rgb(TEXT)))
        })
        .child(label.into())
}

/// `+1.23%` in green, `-1.23%` in red, or a muted dash.
pub fn format_change(change: Option<f64>) -> (String, Rgba) {
    match change {
        Some(c) if c >= 0.0 => (format!("+{c:.2}%"), rgb(GREEN)),
        Some(c) => (format!("{c:.2}%"), rgb(RED)),
        None => ("—".into(), rgb(MUTED)),
    }
}
