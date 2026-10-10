//! Resizable panels: the sidebar, the account panel and the bottom panel,
//! sized by dragging the dividers between them and remembered across
//! launches.

use std::path::PathBuf;

use gpui::{
    Context, Div, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Size, div, prelude::*, px,
    rgb,
};
use serde::{Deserialize, Serialize};

use crate::Warren;
use crate::store::Store;
use crate::theme::*;

/// Panel sizes, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelSizes {
    /// The agents and watchlist sidebar on the left.
    pub left: f32,
    /// The account, positions and orders panel on the right.
    pub right: f32,
    /// The activity, scorecard and usage panel along the bottom.
    pub bottom: f32,
}

impl Default for PanelSizes {
    fn default() -> Self {
        Self {
            left: 200.,
            right: 260.,
            bottom: 150.,
        }
    }
}

/// Room the center (chart) keeps however the panels are dragged.
const MIN_CENTER_WIDTH: f32 = 420.;
const MIN_CENTER_HEIGHT: f32 = 220.;
const LEFT_RANGE: (f32, f32) = (160., 420.);
const RIGHT_RANGE: (f32, f32) = (200., 520.);
const BOTTOM_MIN: f32 = 80.;
/// The divider's grab width; a one-pixel line is drawn in its middle.
const GRIP: f32 = 8.;

impl PanelSizes {
    fn file() -> Option<PathBuf> {
        Store::default_dir().ok().map(|dir| dir.join("layout.json"))
    }

    /// The saved sizes, or the defaults.
    pub fn load() -> Self {
        Self::file()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Some(path) = Self::file()
            && let Ok(text) = serde_json::to_string_pretty(self)
        {
            let _ = std::fs::write(path, text);
        }
    }

    /// These sizes shrunk, if need be, so the center keeps its minimum in a
    /// window of `viewport`. The saved sizes are kept for a bigger window.
    pub fn fit(&self, viewport: Size<Pixels>) -> Self {
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let mut fitted = *self;
        let mut excess = (fitted.left + fitted.right + MIN_CENTER_WIDTH - width).max(0.);
        for (side, min) in [
            (&mut fitted.left, LEFT_RANGE.0),
            (&mut fitted.right, RIGHT_RANGE.0),
        ] {
            let give = excess.min(*side - min).max(0.);
            *side -= give;
            excess -= give;
        }
        let max_bottom = height - crate::HEADER_HEIGHT - MIN_CENTER_HEIGHT;
        fitted.bottom = fitted.bottom.min(max_bottom).max(BOTTOM_MIN);
        fitted
    }

    /// Move `edge` to the pointer at `at` in a window of `viewport`,
    /// keeping every panel within its range and the center usable.
    fn drag(&mut self, edge: Edge, at: gpui::Point<Pixels>, viewport: Size<Pixels>) {
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        match edge {
            Edge::Left => {
                let max = LEFT_RANGE.1.min(width - self.right - MIN_CENTER_WIDTH);
                self.left = x.clamp(LEFT_RANGE.0, max.max(LEFT_RANGE.0));
            }
            Edge::Right => {
                let max = RIGHT_RANGE.1.min(width - self.left - MIN_CENTER_WIDTH);
                self.right = (width - x).clamp(RIGHT_RANGE.0, max.max(RIGHT_RANGE.0));
            }
            Edge::Bottom => {
                let max = height - crate::HEADER_HEIGHT - MIN_CENTER_HEIGHT;
                self.bottom = (height - y).clamp(BOTTOM_MIN, max.max(BOTTOM_MIN));
            }
        }
    }
}

/// A divider being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Bottom,
}

impl Warren {
    /// A draggable divider for `edge`.
    pub fn divider(&self, edge: Edge, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.resizing == Some(edge);
        let line = div()
            .bg(rgb(if active { ACCENT } else { BORDER }))
            .group_hover("divider", |s| s.bg(rgb(MUTED)));
        let grip = match edge {
            Edge::Left | Edge::Right => div()
                .w(px(GRIP))
                .h_full()
                .flex()
                .flex_row()
                .justify_center()
                .cursor_col_resize()
                .child(line.w(px(1.)).h_full()),
            Edge::Bottom => div()
                .h(px(GRIP))
                .w_full()
                .flex()
                .flex_col()
                .justify_center()
                .cursor_row_resize()
                .child(line.h(px(1.)).w_full()),
        };
        grip.id(match edge {
            Edge::Left => "divider-left",
            Edge::Right => "divider-right",
            Edge::Bottom => "divider-bottom",
        })
        .group("divider")
        .flex_none()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.resizing = Some(edge);
                cx.notify();
            }),
        )
    }

    /// Follow the pointer while a divider is held; on release, keep the
    /// sizes for next launch.
    pub fn resize_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edge) = self.resizing else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_resize(cx);
            return;
        }
        let viewport = window.viewport_size();
        // Drag from what's on screen, so a squeezed panel doesn't jump.
        self.panels = self.panels.fit(viewport);
        self.panels.drag(edge, event.position, viewport);
        cx.notify();
    }

    pub fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() {
            self.panels.save();
            cx.notify();
        }
    }
}

/// Wraps the root so drags keep tracking over any panel.
pub fn track_resize(root: Div, cx: &mut Context<Warren>) -> Div {
    root.on_mouse_move(
        cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            this.resize_moved(event, window, cx)
        }),
    )
    .on_mouse_up(
        MouseButton::Left,
        cx.listener(|this, _, _, cx| this.end_resize(cx)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[test]
    fn drags_stay_within_range_and_leave_the_center() {
        let viewport = size(px(1200.), px(760.));
        let mut p = PanelSizes::default();
        p.drag(Edge::Left, point(px(10.), px(0.)), viewport);
        assert_eq!(p.left, LEFT_RANGE.0);
        p.drag(Edge::Left, point(px(300.), px(0.)), viewport);
        assert_eq!(p.left, 300.);
        p.drag(Edge::Right, point(px(400.), px(0.)), viewport);
        // The center keeps 420, so 1200 - 300 - 420 = 480 is the most.
        assert_eq!(p.right, 480.);
        p.drag(Edge::Left, point(px(420.), px(0.)), viewport);
        // Now only 1200 - 480 - 420 = 300 is left for the sidebar.
        assert_eq!(p.left, 300.);
        p.drag(Edge::Bottom, point(px(0.), px(700.)), viewport);
        assert_eq!(p.bottom, BOTTOM_MIN);
        p.drag(Edge::Bottom, point(px(0.), px(100.)), viewport);
        assert_eq!(p.bottom, 760. - crate::HEADER_HEIGHT - MIN_CENTER_HEIGHT);
    }

    #[test]
    fn fitting_a_small_window_squeezes_the_sidebar_first() {
        let p = PanelSizes {
            left: 300.,
            right: 300.,
            bottom: 150.,
        };
        let fitted = p.fit(size(px(900.), px(760.)));
        // 300 + 300 + 420 is 120 too wide; the sidebar gives it up.
        assert_eq!((fitted.left, fitted.right), (180., 300.));
        let fitted = p.fit(size(px(780.), px(760.)));
        assert_eq!((fitted.left, fitted.right), (LEFT_RANGE.0, 200.));
        assert_eq!(p.fit(size(px(1400.), px(760.))), p);
    }
}
