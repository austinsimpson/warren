//! Price history chart: the selectable time ranges and a line/area renderer.

use chrono::{DateTime, Duration, FixedOffset, Local, Utc};
use gpui::{
    Bounds, IntoElement, PathBuilder, Pixels, Point, Styled, canvas, fill, linear_color_stop,
    linear_gradient, point, px, rgb, size,
};

use crate::market::alpaca::{Bar, Timeframe};
use crate::theme::{BORDER, GREEN, MUTED, RED, tint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartRange {
    Day,
    Week,
    Month,
    Year,
}

impl ChartRange {
    pub const ALL: [ChartRange; 4] = [Self::Day, Self::Week, Self::Month, Self::Year];

    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "1D",
            Self::Week => "1W",
            Self::Month => "1M",
            Self::Year => "1Y",
        }
    }

    /// Alpaca bar timeframe for this range.
    pub fn timeframe(self) -> Timeframe {
        match self {
            Self::Day => Timeframe::Minutes(5),
            Self::Week => Timeframe::Minutes(30),
            Self::Month => Timeframe::Hours(1),
            Self::Year => Timeframe::Day,
        }
    }

    /// How far back to request bars. `Day` looks back further than a day so
    /// weekends and holidays still have a session; see [`Self::trim`].
    pub fn start(self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - match self {
            Self::Day => Duration::days(5),
            Self::Week => Duration::days(7),
            Self::Month => Duration::days(30),
            Self::Year => Duration::days(365),
        }
    }

    /// For `Day`, keep only the most recent session. Sessions are grouped by
    /// date at UTC-5, which keeps US pre-market through after-hours (4am–8pm
    /// ET) on one date in both EST and EDT.
    pub fn trim(self, mut bars: Vec<Bar>) -> Vec<Bar> {
        if self == Self::Day {
            let session_tz = FixedOffset::west_opt(5 * 3600).unwrap();
            let session_date = |bar: &Bar| bar.timestamp.with_timezone(&session_tz).date_naive();
            if let Some(last) = bars.last().map(session_date) {
                bars.retain(|bar| session_date(bar) == last);
            }
        }
        bars
    }

    /// Format a bar time for the axis labels, in local time.
    pub fn format_time(self, time: DateTime<Utc>) -> String {
        let local = time.with_timezone(&Local);
        match self {
            Self::Day => local.format("%-I:%M %p").to_string(),
            Self::Week | Self::Month => local.format("%b %-d").to_string(),
            Self::Year => local.format("%b %-d, %Y").to_string(),
        }
    }
}

/// A line chart of closing prices: faint gridlines, a dashed baseline at the
/// first close, a gradient fill, and a marker on the latest price. The
/// research target and stop, `levels`, are marked when they fall within the
/// plotted range. Fills whatever space its parent gives it.
pub fn price_chart(
    closes: Vec<f64>,
    color: u32,
    levels: (Option<f64>, Option<f64>),
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let Some(plot) = Plot::new(&closes, bounds) else {
                return;
            };

            for i in 1..GRID_LINES {
                let y = bounds.top() + bounds.size.height * (i as f32 / GRID_LINES as f32);
                let mut grid = PathBuilder::stroke(px(1.));
                grid.move_to(point(bounds.left(), y));
                grid.line_to(point(bounds.right(), y));
                if let Ok(path) = grid.build() {
                    window.paint_path(path, tint(BORDER, 0.7));
                }
            }

            let baseline_y = plot.y(closes[0]);
            let mut baseline = PathBuilder::stroke(px(1.)).dash_array(&[px(3.), px(3.)]);
            baseline.move_to(point(bounds.left(), baseline_y));
            baseline.line_to(point(bounds.right(), baseline_y));
            if let Ok(path) = baseline.build() {
                window.paint_path(path, tint(MUTED, 0.6));
            }

            let (target, stop) = levels;
            for (level, level_color) in [(target, GREEN), (stop, RED)] {
                let Some(level) = level.filter(|l| (plot.min..=plot.max).contains(l)) else {
                    continue;
                };
                let y = plot.y(level);
                let mut line = PathBuilder::stroke(px(1.)).dash_array(&[px(6.), px(4.)]);
                line.move_to(point(bounds.left(), y));
                line.line_to(point(bounds.right(), y));
                if let Ok(path) = line.build() {
                    window.paint_path(path, tint(level_color, 0.55));
                }
            }

            let points = &plot.points;
            let last = points[points.len() - 1];
            let mut area = PathBuilder::fill();
            area.move_to(point(points[0].x, bounds.bottom()));
            for &p in points {
                area.line_to(p);
            }
            area.line_to(point(last.x, bounds.bottom()));
            area.close();
            if let Ok(path) = area.build() {
                window.paint_path(
                    path,
                    linear_gradient(
                        180.,
                        linear_color_stop(tint(color, 0.22), 0.),
                        linear_color_stop(tint(color, 0.), 1.),
                    ),
                );
            }

            let mut line = PathBuilder::stroke(px(1.5));
            line.move_to(points[0]);
            for &p in &points[1..] {
                line.line_to(p);
            }
            if let Ok(path) = line.build() {
                window.paint_path(path, rgb(color));
            }

            for (radius, alpha) in [(6., 0.18), (3., 1.)] {
                let r = px(radius);
                let dot = Bounds::new(point(last.x - r, last.y - r), size(r * 2., r * 2.));
                window.paint_quad(fill(dot, tint(color, alpha)).corner_radii(r));
            }
        },
    )
    .size_full()
}

const GRID_LINES: usize = 4;

/// One research report on the conviction timeline.
pub struct ConvictionPoint {
    pub at: DateTime<Utc>,
    /// 0–1.
    pub conviction: f64,
    /// Rating color for the marker.
    pub color: u32,
}

/// Conviction over time on a fixed 0–1 scale, with the executor's buy
/// threshold dashed and neutral (0.5) faint. Points are spaced by time and
/// colored by rating; the latest is enlarged. Fills its parent.
pub fn conviction_history(points: Vec<ConvictionPoint>, threshold: f64) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let inset = px(5.);
            let left = bounds.left() + inset;
            let width = bounds.size.width - inset * 2.;
            let top = bounds.top() + inset;
            let height = bounds.size.height - inset * 2.;
            let (lo, hi) = conviction_scale(&points, threshold);
            let y = |value: f64| top + height * (1.0 - ((value - lo) / (hi - lo)) as f32);

            for (level, color, dashed) in [
                (0.5, tint(BORDER, 0.9), false),
                (threshold, tint(GREEN, 0.45), true),
            ] {
                if !(lo..=hi).contains(&level) {
                    continue;
                }
                let mut line = PathBuilder::stroke(px(1.));
                if dashed {
                    line = line.dash_array(&[px(3.), px(3.)]);
                }
                line.move_to(point(left, y(level)));
                line.line_to(point(left + width, y(level)));
                if let Ok(path) = line.build() {
                    window.paint_path(path, color);
                }
            }

            let Some(first) = points.first() else {
                return;
            };
            let start = first.at.timestamp() as f64;
            let span = (points[points.len() - 1].at.timestamp() as f64 - start).max(0.0);
            let x = |at: DateTime<Utc>| {
                if span == 0.0 {
                    left + width
                } else {
                    left + width * ((at.timestamp() as f64 - start) / span) as f32
                }
            };
            let placed: Vec<_> = points
                .iter()
                .map(|p| (point(x(p.at), y(p.conviction)), p.color))
                .collect();

            if placed.len() > 1 {
                let mut line = PathBuilder::stroke(px(1.5));
                line.move_to(placed[0].0);
                for (p, _) in &placed[1..] {
                    line.line_to(*p);
                }
                if let Ok(path) = line.build() {
                    window.paint_path(path, tint(MUTED, 0.9));
                }
            }
            for (i, (p, color)) in placed.iter().enumerate() {
                let r = if i == placed.len() - 1 {
                    px(4.)
                } else {
                    px(2.5)
                };
                let dot = Bounds::new(point(p.x - r, p.y - r), size(r * 2., r * 2.));
                window.paint_quad(fill(dot, rgb(*color)).corner_radii(r));
            }
        },
    )
    .size_full()
}

/// The conviction range to plot: the points and the threshold with some
/// padding, at least `MIN_SPAN` tall so small moves stay legible without
/// noise looking dramatic, and kept within 0–1.
fn conviction_scale(points: &[ConvictionPoint], threshold: f64) -> (f64, f64) {
    const MIN_SPAN: f64 = 0.2;
    const PAD: f64 = 0.04;
    let (lo, hi) = points
        .iter()
        .map(|p| p.conviction)
        .fold((threshold, threshold), |(lo, hi), c| (lo.min(c), hi.max(c)));
    let (mut lo, mut hi) = (lo - PAD, hi + PAD);
    if hi - lo < MIN_SPAN {
        let mid = (lo + hi) / 2.0;
        (lo, hi) = (mid - MIN_SPAN / 2.0, mid + MIN_SPAN / 2.0);
    }
    if lo < 0.0 {
        (lo, hi) = (0.0, (hi - lo).min(1.0));
    } else if hi > 1.0 {
        (lo, hi) = ((1.0 - (hi - lo)).max(0.0), 1.0);
    }
    (lo, hi)
}

/// Closes mapped onto the chart's bounds, with a little vertical headroom.
struct Plot {
    points: Vec<Point<Pixels>>,
    min: f64,
    max: f64,
    bounds: Bounds<Pixels>,
}

impl Plot {
    fn new(closes: &[f64], bounds: Bounds<Pixels>) -> Option<Self> {
        if closes.len() < 2 {
            return None;
        }
        let (min, max) = closes
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &c| (lo.min(c), hi.max(c)));
        let pad = ((max - min) * 0.08).max(max.abs() * 0.001);
        let mut plot = Self {
            points: Vec::with_capacity(closes.len()),
            min: min - pad,
            max: max + pad,
            bounds,
        };
        let step = f32::from(bounds.size.width) / (closes.len() - 1) as f32;
        plot.points = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| point(bounds.left() + px(i as f32 * step), plot.y(c)))
            .collect();
        Some(plot)
    }

    fn y(&self, value: f64) -> Pixels {
        let fraction = ((self.max - value) / (self.max - self.min)) as f32;
        self.bounds.top() + self.bounds.size.height * fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_at(rfc3339: &str) -> Bar {
        Bar {
            timestamp: rfc3339.parse().unwrap(),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0,
            trade_count: 0,
            vwap: 1.0,
        }
    }

    #[test]
    fn conviction_scale_zooms_but_stays_in_range() {
        let at = |c| ConvictionPoint {
            at: Utc::now(),
            conviction: c,
            color: 0,
        };
        let (lo, hi) = conviction_scale(&[at(0.59), at(0.56)], 0.6);
        assert!((hi - lo - 0.2).abs() < 1e-9 && lo < 0.56 && hi > 0.6);
        // Shifted inside 0–1, keeping the padded span.
        let (lo, hi) = conviction_scale(&[at(0.98)], 0.6);
        assert!((lo - 0.54).abs() < 1e-9 && hi == 1.0);
        let (lo, hi) = conviction_scale(&[at(0.01)], 0.05);
        assert!(lo == 0.0 && (hi - 0.2).abs() < 1e-9);
    }

    #[test]
    fn day_keeps_only_last_session() {
        let bars = vec![
            bar_at("2026-10-08T19:55:00Z"), // Thu 3:55pm EDT
            bar_at("2026-10-09T08:00:00Z"), // Fri 4:00am EDT, pre-market
            bar_at("2026-10-09T23:55:00Z"), // Fri 7:55pm EDT, after-hours
        ];
        let trimmed = ChartRange::Day.trim(bars.clone());
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].timestamp, bars[1].timestamp);

        assert_eq!(ChartRange::Week.trim(bars).len(), 3);
    }

    /// Hits the live API; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_bars_for_each_range() {
        dotenvy::dotenv().ok();
        let client = crate::market::alpaca::AlpacaClient::from_env().unwrap();
        for range in ChartRange::ALL {
            let bars = crate::runtime::RUNTIME
                .block_on(client.bars("AAPL", range.timeframe(), range.start(Utc::now())))
                .unwrap();
            let bars = range.trim(bars);
            let (first, last) = (bars.first().unwrap(), bars.last().unwrap());
            println!(
                "{}: {} bars, {} → {} ({:.2} → {:.2})",
                range.label(),
                bars.len(),
                range.format_time(first.timestamp),
                range.format_time(last.timestamp),
                first.close,
                last.close,
            );
            assert!(bars.len() >= 2);
        }
    }
}
