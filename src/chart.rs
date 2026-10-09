//! Price history chart: the selectable time ranges and a line/area renderer.

use chrono::{DateTime, Duration, FixedOffset, Local, Utc};
use gpui::{Bounds, Hsla, IntoElement, PathBuilder, Pixels, Rgba, Styled, canvas, point, px};

use crate::market::alpaca::{Bar, Timeframe};

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

/// A line chart of closing prices with a translucent fill beneath it. Fills
/// whatever space its parent gives it.
pub fn price_chart(closes: Vec<f64>, color: Rgba) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let Some(points) = plot_points(&closes, bounds) else {
                return;
            };

            let mut area = PathBuilder::fill();
            area.move_to(point(points[0].x, bounds.bottom()));
            for &p in &points {
                area.line_to(p);
            }
            area.line_to(point(points[points.len() - 1].x, bounds.bottom()));
            area.close();
            if let Ok(path) = area.build() {
                window.paint_path(path, Hsla::from(color).opacity(0.15));
            }

            let mut line = PathBuilder::stroke(px(2.));
            line.move_to(points[0]);
            for &p in &points[1..] {
                line.line_to(p);
            }
            if let Ok(path) = line.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size_full()
}

/// Map closes onto `bounds`, leaving a little vertical headroom.
fn plot_points(closes: &[f64], bounds: Bounds<Pixels>) -> Option<Vec<gpui::Point<Pixels>>> {
    if closes.len() < 2 {
        return None;
    }
    let (min, max) = closes
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &c| (lo.min(c), hi.max(c)));
    let pad = ((max - min) * 0.05).max(max.abs() * 0.001);
    let (min, max) = (min - pad, max + pad);

    let width = f64::from(bounds.size.width);
    let height = f64::from(bounds.size.height);
    let step = width / (closes.len() - 1) as f64;
    Some(
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let x = i as f64 * step;
                let y = (max - c) / (max - min) * height;
                point(bounds.left() + px(x as f32), bounds.top() + px(y as f32))
            })
            .collect(),
    )
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
