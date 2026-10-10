//! Technical indicators computed from daily bars, so agents reason over exact
//! numbers instead of doing arithmetic on raw price series.

use serde::Serialize;

use crate::market::alpaca::Bar;

/// Trading days per period, for return lookbacks.
const WEEK: usize = 5;
const MONTH: usize = 21;
const QUARTER: usize = 63;
const YEAR: usize = 252;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Technicals {
    pub last_close: f64,
    pub sma_20: Option<f64>,
    pub sma_50: Option<f64>,
    pub sma_200: Option<f64>,
    /// Wilder's 14-day RSI, 0–100.
    pub rsi_14: Option<f64>,
    /// Wilder's 14-day average true range, in dollars.
    pub atr_14: Option<f64>,
    /// ATR as a percent of the last close.
    pub atr_percent: Option<f64>,
    pub return_1w_percent: Option<f64>,
    pub return_1m_percent: Option<f64>,
    pub return_3m_percent: Option<f64>,
    pub return_1y_percent: Option<f64>,
    /// Annualized standard deviation of daily returns over the last 3 months.
    pub volatility_3m_percent: Option<f64>,
    pub high_52w: f64,
    pub low_52w: f64,
    /// Highest high and lowest low of the last 20 bars.
    pub high_20d: f64,
    pub low_20d: f64,
    /// How far the last close is below the 52-week high.
    pub percent_below_52w_high: f64,
    /// Average daily dollar volume over the last 20 days.
    pub avg_dollar_volume_20d: Option<f64>,
    pub bars_used: usize,
}

/// Compute indicators from daily bars, oldest first. `None` with no bars.
pub fn compute(bars: &[Bar]) -> Option<Technicals> {
    let last = bars.last()?;
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let year = &bars[bars.len().saturating_sub(YEAR)..];
    let high_52w = year.iter().map(|b| b.high).fold(f64::MIN, f64::max);
    let low_52w = year.iter().map(|b| b.low).fold(f64::MAX, f64::min);
    let month = &bars[bars.len().saturating_sub(20)..];
    let high_20d = month.iter().map(|b| b.high).fold(f64::MIN, f64::max);
    let low_20d = month.iter().map(|b| b.low).fold(f64::MAX, f64::min);
    let atr_14 = atr(bars, 14);

    Some(Technicals {
        last_close: last.close,
        sma_20: sma(&closes, 20),
        sma_50: sma(&closes, 50),
        sma_200: sma(&closes, 200),
        rsi_14: rsi(&closes, 14),
        atr_14,
        atr_percent: atr_14.map(|atr| atr / last.close * 100.0),
        return_1w_percent: period_return(&closes, WEEK),
        return_1m_percent: period_return(&closes, MONTH),
        return_3m_percent: period_return(&closes, QUARTER),
        return_1y_percent: period_return(&closes, YEAR),
        volatility_3m_percent: volatility(&closes, QUARTER),
        high_52w,
        low_52w,
        high_20d,
        low_20d,
        percent_below_52w_high: (high_52w - last.close) / high_52w * 100.0,
        avg_dollar_volume_20d: (bars.len() >= 20).then(|| {
            bars[bars.len() - 20..]
                .iter()
                .map(|b| b.close * b.volume as f64)
                .sum::<f64>()
                / 20.0
        }),
        bars_used: bars.len(),
    })
}

fn sma(values: &[f64], period: usize) -> Option<f64> {
    (values.len() >= period)
        .then(|| values[values.len() - period..].iter().sum::<f64>() / period as f64)
}

fn period_return(closes: &[f64], days: usize) -> Option<f64> {
    let then = *closes.get(closes.len().checked_sub(days + 1)?)?;
    Some((closes[closes.len() - 1] - then) / then * 100.0)
}

fn rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() <= period {
        return None;
    }
    let changes: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();
    let (mut gain, mut loss) = changes[..period]
        .iter()
        .fold((0.0, 0.0), |(g, l), &c| (g + c.max(0.0), l + (-c).max(0.0)));
    gain /= period as f64;
    loss /= period as f64;
    for &change in &changes[period..] {
        gain = (gain * (period - 1) as f64 + change.max(0.0)) / period as f64;
        loss = (loss * (period - 1) as f64 + (-change).max(0.0)) / period as f64;
    }
    Some(if loss == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + gain / loss)
    })
}

fn atr(bars: &[Bar], period: usize) -> Option<f64> {
    if bars.len() <= period {
        return None;
    }
    let true_ranges: Vec<f64> = bars
        .windows(2)
        .map(|w| {
            let (prev_close, bar) = (w[0].close, &w[1]);
            (bar.high - bar.low)
                .max((bar.high - prev_close).abs())
                .max((bar.low - prev_close).abs())
        })
        .collect();
    let mut atr = true_ranges[..period].iter().sum::<f64>() / period as f64;
    for &tr in &true_ranges[period..] {
        atr = (atr * (period - 1) as f64 + tr) / period as f64;
    }
    Some(atr)
}

fn volatility(closes: &[f64], days: usize) -> Option<f64> {
    if closes.len() < days + 1 {
        return None;
    }
    let returns: Vec<f64> = closes[closes.len() - days - 1..]
        .windows(2)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    Some(variance.sqrt() * (YEAR as f64).sqrt() * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Bar {
                timestamp: chrono::DateTime::from_timestamp(i as i64 * 86_400, 0).unwrap(),
                open: c,
                high: c + 1.0,
                low: c - 1.0,
                close: c,
                volume: 1_000,
                trade_count: 10,
                vwap: c,
            })
            .collect()
    }

    #[test]
    fn rising_series() {
        let closes: Vec<f64> = (1..=260).map(f64::from).collect();
        let t = compute(&bars(&closes)).unwrap();
        assert_eq!(t.last_close, 260.0);
        assert_eq!(t.sma_20, Some(250.5));
        assert_eq!(t.rsi_14, Some(100.0));
        assert_eq!(t.return_1w_percent, Some((260.0 - 255.0) / 255.0 * 100.0));
        assert_eq!(t.high_52w, 261.0);
        // Each bar's true range is max(2, |h - prev c|) = 2.
        assert!((t.atr_14.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(t.avg_dollar_volume_20d, Some(250.5 * 1_000.0));
    }

    #[test]
    fn rsi_balances_on_alternating_moves() {
        let closes: Vec<f64> = (0..50)
            .map(|i| if i % 2 == 0 { 100.0 } else { 101.0 })
            .collect();
        let rsi = rsi(&closes, 14).unwrap();
        assert!((rsi - 50.0).abs() < 5.0, "{rsi}");
    }

    #[test]
    fn short_history_leaves_long_indicators_empty() {
        let t = compute(&bars(&[10.0, 11.0, 12.0])).unwrap();
        assert_eq!(t.sma_20, None);
        assert_eq!(t.rsi_14, None);
        assert_eq!(t.return_1y_percent, None);
        assert_eq!(compute(&[]), None);
    }
}
