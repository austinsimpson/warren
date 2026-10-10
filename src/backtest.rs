//! Backtests the computed half of scoring: replays [`scoring::quant_factors`]
//! over daily history, scoring each stock every `horizon` trading days from
//! only the bars available that day, then measures what happened next.
//!
//! Run with `cargo run --release -- backtest [--horizon DAYS] [SYMBOL ...]`.
//!
//! What this can and can't tell you:
//! - Only the quant factors are tested. Catalyst, news, and setup are judged
//!   by models whose training data covers these years, so replaying them on
//!   past dates would leak the outcome. Forward paper trading tests those.
//! - Conviction here is the equal-weight mean of the four quant factors
//!   alone, so it is not on quite the same footing as a live report's.
//! - The default universe is roughly today's S&P 100. Names that were
//!   dropped or delisted are missing, which flatters every return here
//!   (survivorship bias). Factors are judged mainly against the equal-weight
//!   universe on the same dates, which carries the same bias, so their
//!   relative edge is more trustworthy than any absolute return.
//! - Samples are `horizon` days apart, so forward windows don't overlap.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use chrono::{NaiveDate, Utc};
use futures::{StreamExt as _, stream};

use crate::agents::{
    scoring::{self, QuantFactors},
    technicals::{self, Technicals},
    tools::BENCHMARK,
};
use crate::market::alpaca::{AlpacaClient, Bar};

/// The earliest SIP history Alpaca serves.
const HISTORY_START: &str = "2016-01-01T00:00:00Z";
/// Calendar days of bars behind each score, as in live research.
const LOOKBACK_DAYS: i64 = 550;
/// Trading days each position is held, and the spacing between samples,
/// unless `--horizon` says otherwise: two weeks, the swing horizon.
const DEFAULT_HORIZON: usize = 10;
/// Slippage and fees per side, as a fraction of price.
const COST: f64 = 0.0005;
/// Stop and target for simulated trades, in ATRs from the entry. Live levels
/// come from the research agent; these stand in for them.
const STOP_ATR: f64 = 2.0;
const TARGET_ATR: f64 = 3.0;
/// Quant-only conviction needed to take a simulated trade.
const THRESHOLDS: [f64; 3] = [0.5, 0.6, 0.7];

const FACTORS: [&str; 4] = ["trend", "momentum", "rel strength", "extension"];

/// Large caps across sectors, approximately today's S&P 100.
const UNIVERSE: &[&str] = &[
    "AAPL", "ABBV", "ABT", "ACN", "ADBE", "AIG", "AMD", "AMGN", "AMT", "AMZN", "AVGO", "AXP", "BA",
    "BAC", "BK", "BKNG", "BLK", "BMY", "BRK.B", "C", "CAT", "CHTR", "CL", "CMCSA", "COF", "COP",
    "COST", "CRM", "CSCO", "CVS", "CVX", "DE", "DHR", "DIS", "DUK", "EMR", "F", "FDX", "GD", "GE",
    "GILD", "GM", "GOOGL", "GS", "HD", "HON", "IBM", "INTC", "INTU", "ISRG", "JNJ", "JPM", "KO",
    "LIN", "LLY", "LMT", "LOW", "MA", "MCD", "MDLZ", "MDT", "MET", "META", "MMM", "MO", "MRK",
    "MS", "MSFT", "NEE", "NFLX", "NKE", "NOW", "NVDA", "ORCL", "PEP", "PFE", "PG", "PLTR", "PM",
    "PYPL", "QCOM", "RTX", "SBUX", "SCHW", "SO", "SPG", "T", "TGT", "TMO", "TMUS", "TSLA", "TXN",
    "UBER", "UNH", "UNP", "UPS", "USB", "V", "VZ", "WFC", "WMT", "XOM",
];

/// Entry point for `warren backtest [--horizon DAYS] [SYMBOL ...]`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut horizon = DEFAULT_HORIZON;
    let mut args = args.to_vec();
    if let Some(at) = args.iter().position(|a| a == "--horizon") {
        let days = args
            .get(at + 1)
            .context("--horizon needs a number of days")?;
        horizon = days.parse().context("--horizon needs a number of days")?;
        if horizon == 0 {
            bail!("--horizon must be at least 1 day");
        }
        args.drain(at..at + 2);
    }
    let symbols: Vec<String> = if args.is_empty() {
        UNIVERSE.iter().map(|s| s.to_string()).collect()
    } else {
        args.iter().map(|s| s.trim().to_uppercase()).collect()
    };
    let alpaca = AlpacaClient::from_env()?;
    let cache = BarCache::today()?;
    let (benchmark, stocks) = crate::runtime::RUNTIME.block_on(fetch(&alpaca, &cache, symbols))?;
    let observations = observe_all(&benchmark, &stocks, horizon);
    if observations.is_empty() {
        bail!("no observations; is there enough history?");
    }
    print!("{}", report(&observations, stocks.len(), horizon));
    Ok(())
}

/// Daily bars for SPY and each symbol. Symbols that fail are skipped.
async fn fetch(
    alpaca: &AlpacaClient,
    cache: &BarCache,
    symbols: Vec<String>,
) -> Result<(Vec<Bar>, Vec<Series>)> {
    let benchmark = cache
        .history(alpaca, BENCHMARK)
        .await
        .context("fetching benchmark bars")?;
    let total = symbols.len();
    let mut stocks: Vec<Series> = stream::iter(symbols)
        .map(|symbol| async move {
            let bars = cache.history(alpaca, &symbol).await;
            (symbol, bars)
        })
        .buffer_unordered(8)
        .filter_map(|(symbol, bars)| async move {
            match bars {
                Ok(bars) if !bars.is_empty() => Some(Series { symbol, bars }),
                Ok(_) => {
                    eprintln!("{symbol}: no bars, skipped");
                    None
                }
                Err(err) => {
                    eprintln!("{symbol}: {err:#}, skipped");
                    None
                }
            }
        })
        .collect()
        .await;
    stocks.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    eprintln!("fetched {} of {total} symbols", stocks.len());
    Ok((benchmark, stocks))
}

/// Today's daily history per symbol, kept on disk so reruns with other
/// settings don't refetch it. Earlier days' caches are deleted.
struct BarCache {
    dir: PathBuf,
}

impl BarCache {
    fn today() -> Result<Self> {
        let root = dirs::cache_dir()
            .context("no cache directory on this platform")?
            .join("warren")
            .join("bars");
        let dir = root.join(Utc::now().format("%Y-%m-%d").to_string());
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for entry in fs::read_dir(&root)?.flatten() {
            if entry.path() != dir {
                fs::remove_dir_all(entry.path()).ok();
            }
        }
        Ok(Self { dir })
    }

    async fn history(&self, alpaca: &AlpacaClient, symbol: &str) -> Result<Vec<Bar>> {
        let path = self.dir.join(format!("{symbol}.json"));
        if let Some(bars) = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            return Ok(bars);
        }
        let bars = alpaca.daily_history(symbol, HISTORY_START.parse()?).await?;
        fs::write(&path, serde_json::to_vec(&bars)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(bars)
    }
}

struct Series {
    symbol: String,
    bars: Vec<Bar>,
}

#[derive(Debug, Clone, PartialEq)]
struct Observation {
    date: NaiveDate,
    /// Trend, momentum, relative strength, extension.
    scores: [u8; 4],
    /// Quant-only conviction, 0–1.
    conviction: f64,
    /// Next open to the close `horizon` bars after the sample, before costs.
    forward: f64,
    /// SPY over the same window.
    benchmark_forward: f64,
    /// `forward` after costs: a position held the full window.
    held: f64,
    /// A simulated trade over the same window with stop and target exits,
    /// after costs.
    bracketed: f64,
    /// `forward` minus the universe's mean `forward` on the same date; set
    /// once every symbol is observed.
    excess: f64,
}

fn date(bar: &Bar) -> NaiveDate {
    bar.timestamp.date_naive()
}

/// The bars in the live research window ending at `end` (inclusive), or
/// `None` if the history doesn't reach back that far, so early samples
/// aren't scored from a fraction of the data live research sees.
fn window(bars: &[Bar], end: usize) -> Option<&[Bar]> {
    let from = date(&bars[end]) - chrono::Duration::days(LOOKBACK_DAYS);
    // A week of slack for holidays at the start of the history.
    if date(&bars[0]) > from + chrono::Duration::days(7) {
        return None;
    }
    let start = bars[..end].partition_point(|b| date(b) <= from);
    Some(&bars[start..=end])
}

/// Sample every symbol on every `horizon`th benchmark trading day.
fn observe_all(benchmark: &[Bar], stocks: &[Series], horizon: usize) -> Vec<Observation> {
    let mut observations = Vec::new();
    let mut i = 0;
    while i + horizon < benchmark.len() {
        let day = date(&benchmark[i]);
        let bench_now = window(benchmark, i).and_then(technicals::compute);
        let bench_forward = benchmark[i + horizon].close / benchmark[i + 1].open - 1.0;
        let first = observations.len();
        for stock in stocks {
            let Ok(j) = stock.bars.binary_search_by_key(&day, date) else {
                continue;
            };
            if let Some(obs) = observe(&stock.bars, j, horizon, bench_now.as_ref(), bench_forward) {
                observations.push(obs);
            }
        }
        let today = &mut observations[first..];
        if !today.is_empty() {
            let mean = today.iter().map(|o| o.forward).sum::<f64>() / today.len() as f64;
            for obs in today {
                obs.excess = obs.forward - mean;
            }
        }
        i += horizon;
    }
    observations
}

/// Score the stock at bar `j` from bars up to and including `j`, then follow
/// it for `horizon` bars. `None` unless all four factors can be scored and
/// the full window lies ahead.
fn observe(
    bars: &[Bar],
    j: usize,
    horizon: usize,
    benchmark: Option<&Technicals>,
    benchmark_forward: f64,
) -> Option<Observation> {
    if j + horizon >= bars.len() {
        return None;
    }
    let now = technicals::compute(window(bars, j)?)?;
    let QuantFactors {
        trend,
        momentum,
        relative_strength,
        extension,
    } = scoring::quant_factors(&now, benchmark);
    let scores = [
        trend?.score,
        momentum?.score,
        relative_strength?.score,
        extension?.score,
    ];
    let mean = scores.iter().map(|&s| f64::from(s)).sum::<f64>() / 4.0;
    let ahead = &bars[j + 1..=j + horizon];
    let forward = ahead[horizon - 1].close / ahead[0].open - 1.0;
    Some(Observation {
        date: date(&bars[j]),
        scores,
        conviction: (mean - 1.0) / 4.0,
        forward,
        held: (1.0 + forward) * (1.0 - COST) / (1.0 + COST) - 1.0,
        benchmark_forward,
        bracketed: simulate_trade(ahead, now.atr_14?),
        excess: 0.0,
    })
}

/// Buy at the first bar's open, then exit at the stop, the target, or the
/// last close, whichever comes first. A bar that touches both is assumed to
/// hit the stop, and gaps past a level fill at the open.
fn simulate_trade(ahead: &[Bar], atr: f64) -> f64 {
    let entry = ahead[0].open;
    let (stop, target) = (entry - STOP_ATR * atr, entry + TARGET_ATR * atr);
    let exit = ahead
        .iter()
        .find_map(|bar| {
            if bar.low <= stop {
                Some(bar.open.min(stop))
            } else if bar.high >= target {
                Some(bar.open.max(target))
            } else {
                None
            }
        })
        .unwrap_or(ahead[ahead.len() - 1].close);
    exit * (1.0 - COST) / (entry * (1.0 + COST)) - 1.0
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn mean(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values
        .into_iter()
        .fold((0.0, 0usize), |(sum, n), v| (sum + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// Mean and t-statistic of a series of independent samples.
fn t_stat(values: &[f64]) -> Option<(f64, f64)> {
    if values.len() < 3 {
        return None;
    }
    let n = values.len() as f64;
    let m = values.iter().sum::<f64>() / n;
    let sd = (values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    Some((m, if sd > 0.0 { m / (sd / n.sqrt()) } else { 0.0 }))
}

fn pct(x: f64) -> String {
    format!("{:+.2}%", x * 100.0)
}

/// One row of a bucket table.
fn bucket_row(label: &str, group: &[&Observation]) -> String {
    let excess = mean(group.iter().map(|o| o.excess)).unwrap_or(0.0);
    let vs_spy = mean(group.iter().map(|o| o.forward - o.benchmark_forward)).unwrap_or(0.0);
    let beat = group.iter().filter(|o| o.excess > 0.0).count() as f64 / group.len().max(1) as f64;
    format!(
        "  {label:<10} {:>7} {:>11} {:>8.0}% {:>10}\n",
        group.len(),
        pct(excess),
        beat * 100.0,
        pct(vs_spy)
    )
}

const BUCKET_HEADER: &str = "             count  vs universe  beat it     vs SPY\n";

/// Per-date mean excess of `high` minus `low` observations, as a t-test over
/// dates (observations on one date are correlated; dates are not).
fn spread(
    observations: &[Observation],
    high: impl Fn(&Observation) -> bool,
    low: impl Fn(&Observation) -> bool,
) -> Option<(f64, f64)> {
    let mut by_date: BTreeMap<NaiveDate, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for obs in observations {
        let entry = by_date.entry(obs.date).or_default();
        if high(obs) {
            entry.0.push(obs.excess);
        } else if low(obs) {
            entry.1.push(obs.excess);
        }
    }
    let spreads: Vec<f64> = by_date
        .values()
        .filter_map(|(h, l)| Some(mean(h.iter().copied())? - mean(l.iter().copied())?))
        .collect();
    t_stat(&spreads)
}

fn report(observations: &[Observation], symbols: usize, horizon: usize) -> String {
    let dates: Vec<NaiveDate> = {
        let mut d: Vec<_> = observations.iter().map(|o| o.date).collect();
        d.dedup();
        d
    };
    let midpoint = dates[dates.len() / 2];
    let mut out = format!(
        "Quant factor backtest: {symbols} symbols, {} to {}, {} dates, {} observations\n\
         Held {horizon} trading days from the next open. \"vs universe\" is the mean \
         return above the\nequal-weight universe on the same dates; \"beat it\" is how \
         often. Spreads are per-date\nmeans of high (4–5) minus low (1–2) scores, with \
         t-statistics over dates (|t| > 2 is\nworth noticing).\n",
        dates[0],
        dates[dates.len() - 1],
        dates.len(),
        observations.len(),
    );

    for (k, name) in FACTORS.iter().enumerate() {
        out += &format!("\n{name}\n{BUCKET_HEADER}");
        for score in 1..=5u8 {
            let group: Vec<_> = observations
                .iter()
                .filter(|o| o.scores[k] == score)
                .collect();
            if !group.is_empty() {
                out += &bucket_row(&format!("score {score}"), &group);
            }
        }
        let halves = [
            ("all", observations.to_vec()),
            (
                "first half",
                observations
                    .iter()
                    .filter(|o| o.date < midpoint)
                    .cloned()
                    .collect(),
            ),
            (
                "second half",
                observations
                    .iter()
                    .filter(|o| o.date >= midpoint)
                    .cloned()
                    .collect(),
            ),
        ];
        let spreads: Vec<String> = halves
            .iter()
            .filter_map(|(label, obs)| {
                let (m, t) = spread(obs, |o| o.scores[k] >= 4, |o| o.scores[k] <= 2)?;
                Some(format!("{label} {} (t {t:.1})", pct(m)))
            })
            .collect();
        out += &format!("  spread     {}\n", spreads.join(", "));
    }

    out += &format!("\nquant conviction\n{BUCKET_HEADER}");
    let bands = [
        ("< 0.40", 0.0, 0.4),
        ("0.40–0.50", 0.4, 0.5),
        ("0.50–0.60", 0.5, 0.6),
        ("0.60–0.70", 0.6, 0.7),
        ("0.70–0.80", 0.7, 0.8),
        (">= 0.80", 0.8, 1.01),
    ];
    for (label, lo, hi) in bands {
        let group: Vec<_> = observations
            .iter()
            .filter(|o| o.conviction >= lo - 1e-9 && o.conviction < hi - 1e-9)
            .collect();
        if !group.is_empty() {
            out += &bucket_row(label, &group);
        }
    }
    if let Some((m, t)) = spread(
        observations,
        |o| o.conviction >= 0.6,
        |o| o.conviction < 0.5,
    ) {
        out += &format!("  spread     >= 0.60 minus < 0.50: {} (t {t:.1})\n", pct(m));
    }

    out += &format!(
        "\nstrategy: every {horizon} days, buy each name at or above the threshold in \
         equal weight,\nthen either bracket the trade (exit at a {STOP_ATR}-ATR stop, a {TARGET_ATR}-ATR \
         target, or after\n{horizon} days) or hold for the full {horizon} days; {:.0} bps per side.\n\
         Uninvested periods earn nothing. Universe and SPY are buy-and-hold, \
         before costs.\n\n{:<17} {:>8} {:>8} {:>7} {:>8} {:>7} {:>8} {:>9}\n",
        COST * 10_000.0,
        "  exit, min conv",
        "CAGR",
        "max DD",
        "Sharpe",
        "in mkt",
        "names",
        "trades",
        "win rate",
    );
    let periods = by_period(observations);
    let years = (dates.len() * horizon) as f64 / 252.0;
    let exits: [(&str, fn(&Observation) -> f64); 2] =
        [("bracket", |o| o.bracketed), ("hold", |o| o.held)];
    for ((exit, outcome), threshold) in exits
        .iter()
        .flat_map(|e| THRESHOLDS.iter().map(move |t| (e, *t)))
    {
        let mut trades: Vec<f64> = Vec::new();
        let returns: Vec<f64> = periods
            .iter()
            .map(|(_, obs)| {
                let picked: Vec<f64> = obs
                    .iter()
                    .filter(|o| o.conviction >= threshold - 1e-9)
                    .map(|o| outcome(o))
                    .collect();
                trades.extend(&picked);
                mean(picked).unwrap_or(0.0)
            })
            .collect();
        let invested = periods
            .iter()
            .filter(|(_, obs)| obs.iter().any(|o| o.conviction >= threshold - 1e-9))
            .count();
        let wins = trades.iter().filter(|&&t| t > 0.0).count();
        out += &format!(
            "  {exit:<7} >= {threshold:.2} {} {:>7.0}% {:>7.1} {:>8} {:>8.0}%\n",
            curve_stats(&returns, years, horizon),
            invested as f64 / periods.len() as f64 * 100.0,
            trades.len() as f64 / periods.len() as f64,
            trades.len(),
            wins as f64 / trades.len().max(1) as f64 * 100.0,
        );
    }
    let universe: Vec<f64> = periods
        .iter()
        .map(|(_, obs)| mean(obs.iter().map(|o| o.forward)).unwrap_or(0.0))
        .collect();
    let spy: Vec<f64> = periods
        .iter()
        .map(|(_, obs)| obs[0].benchmark_forward)
        .collect();
    out += &format!(
        "{:<17} {}\n",
        "  universe",
        curve_stats(&universe, years, horizon)
    );
    out += &format!("{:<17} {}\n", "  SPY", curve_stats(&spy, years, horizon));
    out
}

/// Observations grouped by sample date, in order.
fn by_period(observations: &[Observation]) -> Vec<(NaiveDate, Vec<&Observation>)> {
    let mut periods: BTreeMap<NaiveDate, Vec<&Observation>> = BTreeMap::new();
    for obs in observations {
        periods.entry(obs.date).or_default().push(obs);
    }
    periods.into_iter().collect()
}

/// CAGR, max drawdown, and annualized Sharpe of compounded period returns.
fn curve_stats(returns: &[f64], years: f64, horizon: usize) -> String {
    let (mut equity, mut peak, mut drawdown) = (1.0f64, 1.0f64, 0.0f64);
    for r in returns {
        equity *= 1.0 + r;
        peak = peak.max(equity);
        drawdown = drawdown.max(1.0 - equity / peak);
    }
    let cagr = equity.powf(1.0 / years) - 1.0;
    let sharpe = t_stat(returns).map_or(0.0, |(m, t)| {
        // t = m / (sd / sqrt(n)), so sd = m * sqrt(n) / t.
        let sd = if t != 0.0 {
            m * (returns.len() as f64).sqrt() / t
        } else {
            f64::INFINITY
        };
        m / sd * (252.0 / horizon as f64).sqrt()
    });
    format!(
        "{:>7.1}% {:>7.1}% {sharpe:>7.2}",
        cagr * 100.0,
        -drawdown * 100.0
    )
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn bar(day: i64, open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            timestamp: "2020-01-01T05:00:00Z".parse::<DateTime<Utc>>().unwrap()
                + chrono::Duration::days(day),
            open,
            high,
            low,
            close,
            volume: 1_000_000,
            trade_count: 0,
            vwap: close,
        }
    }

    /// A steady uptrend with some wobble, one bar per calendar day.
    fn series(days: i64) -> Vec<Bar> {
        (0..days)
            .map(|d| {
                let c = 100.0 + d as f64 * 0.2 + (d as f64 * 0.7).sin() * 2.0;
                bar(d, c - 0.3, c + 1.0, c - 1.0, c)
            })
            .collect()
    }

    #[test]
    fn scores_ignore_bars_after_the_sample() {
        let bars = series(800);
        let j = 700;
        let bench = window(&bars, j).and_then(technicals::compute);
        let before = observe(&bars, j, DEFAULT_HORIZON, bench.as_ref(), 0.0).unwrap();

        // Rewrite everything after the sample: scores must not move.
        let mut altered = bars.clone();
        for b in &mut altered[j + 1..] {
            b.high *= 3.0;
            b.close *= 3.0;
        }
        let after = observe(&altered, j, DEFAULT_HORIZON, bench.as_ref(), 0.0).unwrap();
        assert_eq!(before.scores, after.scores);
        assert_eq!(before.conviction, after.conviction);
        assert_ne!(before.forward, after.forward);
    }

    #[test]
    fn window_matches_live_lookback() {
        let bars = series(1000);
        let w = window(&bars, 900).unwrap();
        assert_eq!(date(w.last().unwrap()), date(&bars[900]));
        assert_eq!(
            (date(&bars[900]) - date(&w[0])).num_days(),
            LOOKBACK_DAYS - 1
        );
        // Too little history behind the sample.
        assert!(window(&bars, 500).is_none());
    }

    #[test]
    fn trades_exit_at_stop_target_or_time() {
        let cost = (1.0 - COST) / (1.0 + COST);
        // Target (100 + 3) hit on the second bar.
        let up = [
            bar(0, 100.0, 101.0, 99.5, 100.5),
            bar(1, 101.0, 104.0, 100.0, 103.5),
        ];
        assert!((simulate_trade(&up, 1.0) - (103.0 * cost / 100.0 - 1.0)).abs() < 1e-9);
        // A bar touching both levels counts as the stop.
        let both = [bar(0, 100.0, 104.0, 97.0, 100.0)];
        assert!((simulate_trade(&both, 1.0) - (98.0 * cost / 100.0 - 1.0)).abs() < 1e-9);
        // A gap below the stop fills at the open.
        let gap = [
            bar(0, 100.0, 100.5, 99.0, 99.5),
            bar(1, 95.0, 96.0, 94.0, 95.5),
        ];
        assert!((simulate_trade(&gap, 1.0) - (95.0 * cost / 100.0 - 1.0)).abs() < 1e-9);
        // Neither: the last close.
        let flat = [
            bar(0, 100.0, 101.0, 99.0, 100.0),
            bar(1, 100.0, 101.0, 99.0, 100.7),
        ];
        assert!((simulate_trade(&flat, 1.0) - (100.7 * cost / 100.0 - 1.0)).abs() < 1e-9);
    }

    #[test]
    fn excess_is_relative_to_same_date_universe() {
        let bench = series(800);
        let a = Series {
            symbol: "A".into(),
            bars: series(800),
        };
        let mut b_bars = series(800);
        for bar in &mut b_bars {
            bar.close *= 1.0 + 0.0001 * (date(bar) - date(&bench[0])).num_days() as f64;
        }
        let b = Series {
            symbol: "B".into(),
            bars: b_bars,
        };
        let observations = observe_all(&bench, &[a, b], DEFAULT_HORIZON);
        assert!(!observations.is_empty());
        for (_, obs) in by_period(&observations) {
            let total: f64 = obs.iter().map(|o| o.excess).sum();
            assert!(total.abs() < 1e-12);
        }
    }

    /// Runs the default backtest against live Alpaca data; see the module
    /// docs. `cargo test live_backtest --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_backtest() {
        dotenvy::dotenv().ok();
        run_cli(&[]).unwrap();
    }
}
