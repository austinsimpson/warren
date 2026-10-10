//! Backtests the computed half of scoring: replays [`scoring::quant_factors`]
//! over daily history, scoring each stock every `horizon` trading days from
//! only the bars available that day, then measures what happened next.
//!
//! Run with `cargo run --release -- backtest [--horizon DAYS]
//! [--universe large|screened|liquid] [--fundamentals] [SYMBOL ...]`.
//! `--fundamentals` measures growth indicators from SEC filings instead of
//! price indicators.
//!
//! Universes:
//! - `large` (the default): roughly today's S&P 100, every name on every
//!   sample date.
//! - `screened`: what exploration would have seen. Each sample date
//!   rebuilds its screen from that day's bars across every listed US stock:
//!   names above exploration's price and dollar-volume floors, then the most
//!   active by volume plus the biggest gainers and losers. Exploration picks a
//!   few of these by judgment; this tests all of them.
//!
//! What this can and can't tell you:
//! - Only the quant factors are tested. Catalyst, news, and setup are judged
//!   by models whose training data covers these years, so replaying them on
//!   past dates would leak the outcome. Forward paper trading tests those.
//! - Conviction here is the equal-weight mean of the four quant factors
//!   alone, so it is not on quite the same footing as a live report's.
//! - Both universes start from today's listings, so names since delisted
//!   are missing (survivorship bias). That flatters returns, most of all
//!   among screened small and mid caps, where failures are common. Factors
//!   are judged mainly against the rest of the universe on the same dates,
//!   which carries the same bias, so their relative edge is more trustworthy
//!   than any absolute return.
//! - Scoring needs the same 550 days of history live research uses, so
//!   recent listings join only once they have it.
//! - Samples are `horizon` days apart, so forward windows don't overlap.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use futures::{StreamExt as _, stream};

use crate::agents::{
    scoring::{self, QuantFactors},
    technicals::{self, Technicals},
    tools::BENCHMARK,
};
use crate::config::Config;
use crate::market::alpaca::{AlpacaClient, Bar, Timeframe};

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

/// Candidate indicators from the literature, measured but not scored. Each
/// is ranked into quintiles across the names sampled on a date (1 = lowest
/// value), so no thresholds need choosing. With several candidates, some
/// will look good by chance: trust one only if both halves of the history
/// agree.
const PRICE_INDICATORS: [Indicator; 6] = [
    Indicator {
        name: "1-week return",
        expect: "short-term reversal: lowest quintile best",
    },
    Indicator {
        name: "near 52-week high",
        expect: "anchoring: highest quintile (closest to the high) best",
    },
    Indicator {
        name: "12-1 month momentum",
        expect: "momentum: highest quintile best, over months more than days",
    },
    Indicator {
        name: "relative volume",
        expect: "attention: unclear sign; high volume often precedes reversal",
    },
    Indicator {
        name: "volatility",
        expect: "low-volatility anomaly: lowest quintile best risk-adjusted",
    },
    Indicator {
        name: "composite",
        expect: "12-1 momentum + near the 52-week high + not extended: highest best",
    },
];

/// Index of the composite in [`PRICE_INDICATORS`]: the sum of the 12-1
/// momentum and 52-week-high quintiles and the extension score, set after
/// quintiles.
const COMPOSITE: usize = 5;

/// Growth indicators from SEC fundamentals, with each quarter treated as
/// public 90 days after it ends (`fundamentals::BACKTEST_LAG_DAYS`).
const GROWTH_INDICATORS: [Indicator; 8] = [
    Indicator {
        name: "revenue growth",
        expect: "growth: highest quintile best",
    },
    Indicator {
        name: "growth acceleration",
        expect: "accelerating growth: highest quintile best",
    },
    Indicator {
        name: "gross margin",
        expect: "quality: highest quintile best",
    },
    Indicator {
        name: "gross margin change",
        expect: "improving economics: highest quintile best",
    },
    Indicator {
        name: "share growth",
        expect: "dilution: lowest quintile best",
    },
    Indicator {
        name: "price/sales",
        expect: "valuation: lowest quintile cheapest; growth names often stay expensive",
    },
    Indicator {
        name: "12-1 month momentum",
        expect: "momentum: highest quintile best",
    },
    Indicator {
        name: "growth composite",
        expect: "revenue growth + acceleration + cheapness (price/sales, reversed) + 12-1 \
                 momentum, among gross margins of 40%+: highest best",
    },
];

/// Gross margin, in percent, below which the growth composite isn't scored.
pub(crate) const MIN_GROWTH_GROSS_MARGIN: f64 = 40.0;

/// Which indicators a run measures, and how to compute them.
pub(crate) struct IndicatorSet<'a> {
    list: &'static [Indicator],
    /// Index of the composite, filled in from the other quintiles.
    composite: usize,
    /// Each ticker's fundamentals, for the growth set.
    fundamentals: Option<&'a HashMap<String, crate::fundamentals::Series>>,
}

impl<'a> IndicatorSet<'a> {
    pub(crate) fn price() -> Self {
        Self {
            list: &PRICE_INDICATORS,
            composite: COMPOSITE,
            fundamentals: None,
        }
    }

    pub(crate) fn growth(fundamentals: &'a HashMap<String, crate::fundamentals::Series>) -> Self {
        Self {
            list: &GROWTH_INDICATORS,
            composite: 7,
            fundamentals: Some(fundamentals),
        }
    }

    /// Raw values for `symbol` at bar `j`, the composite left NaN.
    fn raw(&self, symbol: &str, bars: &[Bar], j: usize) -> Vec<f64> {
        let Some(fundamentals) = self.fundamentals else {
            return price_indicators(bars, j).to_vec();
        };
        let nan = vec![f64::NAN; self.list.len()];
        let Some(series) = fundamentals.get(symbol) else {
            return nan;
        };
        let m = series.metrics(date(&bars[j]));
        let (_, price_to_sales) = m.valuation(bars[j].close);
        let or_nan = |v: Option<f64>| v.unwrap_or(f64::NAN);
        vec![
            or_nan(m.revenue_growth_percent),
            or_nan(m.acceleration_points),
            or_nan(m.gross_margin_percent),
            or_nan(m.gross_margin_change_points),
            or_nan(m.share_growth_percent),
            or_nan(price_to_sales),
            price_indicators(bars, j)[2],
            f64::NAN,
        ]
    }

    /// The composite from an observation's other quintiles, or NaN.
    fn combine(&self, obs: &Observation) -> f64 {
        let q = &obs.quintiles;
        if self.fundamentals.is_some() {
            // Growth, acceleration, cheapness, and momentum.
            // Only businesses with growth-company margins, so thin-margin
            // cyclicals with commodity-driven revenue don't rank as growth.
            let quality = obs.indicators[2] >= MIN_GROWTH_GROSS_MARGIN;
            if quality && [q[0], q[1], q[5], q[6]].iter().all(|&x| x > 0) {
                f64::from(q[0] + q[1] + (6 - q[5]) + q[6])
            } else {
                f64::NAN
            }
        } else if q[1] > 0 && q[2] > 0 {
            f64::from(q[1] + q[2] + obs.scores[3])
        } else {
            f64::NAN
        }
    }
}
struct Indicator {
    name: &'static str,
    expect: &'static str,
}

/// Raw price indicator values at bar `j`, in [`PRICE_INDICATORS`] order;
/// NaN where the history is too short.
fn price_indicators(bars: &[Bar], j: usize) -> [f64; 6] {
    let close = |k: usize| j.checked_sub(k).map_or(f64::NAN, |i| bars[i].close);
    let week = close(0) / close(5) - 1.0;
    let year = &bars[j.saturating_sub(251)..=j];
    let near_high = close(0) / year.iter().map(|b| b.high).fold(f64::MIN, f64::max);
    // Twelve months to one month ago, skipping the reversal-prone last month.
    let momentum = close(21) / close(252) - 1.0;
    let relative_volume = match j.checked_sub(20) {
        Some(from) => {
            let prior = bars[from..j].iter().map(|b| b.volume as f64).sum::<f64>() / 20.0;
            bars[j].volume as f64 / prior
        }
        None => f64::NAN,
    };
    let volatility = match j.checked_sub(63) {
        Some(from) => {
            let returns: Vec<f64> = bars[from..=j]
                .windows(2)
                .map(|w| (w[1].close / w[0].close).ln())
                .collect();
            let m = returns.iter().sum::<f64>() / returns.len() as f64;
            (returns.iter().map(|r| (r - m).powi(2)).sum::<f64>() / (returns.len() - 1) as f64)
                .sqrt()
        }
        None => f64::NAN,
    };
    // The composite needs every name's quintiles; see `observe_all`.
    [
        week,
        near_high,
        momentum,
        relative_volume,
        volatility,
        f64::NAN,
    ]
}

/// Quintile (1–5) of each finite value among `values`; 0 for NaN.
pub(crate) fn quintiles(values: &[f64]) -> Vec<u8> {
    let mut order: Vec<usize> = (0..values.len())
        .filter(|&i| values[i].is_finite())
        .collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut out = vec![0; values.len()];
    let n = order.len();
    for (rank, &i) in order.iter().enumerate() {
        out[i] = (rank * 5 / n.max(1)) as u8 + 1;
    }
    out
}

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

/// Removes `--name VALUE` from `args` and returns the value.
fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(at) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    let value = args
        .get(at + 1)
        .with_context(|| format!("{name} needs a value"))?
        .clone();
    args.drain(at..at + 2);
    Ok(Some(value))
}

/// Entry point for `warren backtest`; see the module docs.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    let horizon = match take_flag(&mut args, "--horizon")? {
        Some(days) => days.parse().context("--horizon needs a number of days")?,
        None => DEFAULT_HORIZON,
    };
    if horizon == 0 {
        bail!("--horizon must be at least 1 day");
    }
    let universe = take_flag(&mut args, "--universe")?.unwrap_or_else(|| "large".into());
    let use_fundamentals = match args.iter().position(|a| a == "--fundamentals") {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    };

    let alpaca = AlpacaClient::from_env()?;
    let cache = BarCache::today()?;
    let runtime = &crate::runtime::RUNTIME;
    let benchmark = runtime
        .block_on(cache.history(&alpaca, BENCHMARK))
        .context("fetching benchmark bars")?;
    let (stocks, screen, description) = match universe.as_str() {
        "large" => {
            let symbols: Vec<String> = if args.is_empty() {
                UNIVERSE.iter().map(|s| s.to_string()).collect()
            } else {
                args.iter().map(|s| s.trim().to_uppercase()).collect()
            };
            let stocks = runtime.block_on(fetch(&alpaca, &cache, symbols));
            let description = format!("{} large caps", stocks.len());
            (stocks, None, description)
        }
        "screened" => {
            let config = Config::load_or_create()?;
            let screen = Screen {
                min_price: config.exploration.min_price,
                min_dollar_volume: config.exploration.min_avg_dollar_volume,
                ..Screen::default()
            };
            let stocks = runtime.block_on(fetch_screened_pool(&alpaca, &cache, &screen))?;
            let description = format!(
                "exploration's screen over {} listed stocks (top {} by volume plus top {} \
                 gainers and losers, price >= ${:.0}, 20-day dollar volume >= ${:.0}M)",
                stocks.len(),
                screen.most_active,
                screen.movers,
                screen.min_price,
                screen.min_dollar_volume / 1e6,
            );
            (stocks, Some(screen), description)
        }
        "liquid" => {
            // Every stock above exploration's floors, not just the day's
            // most active and biggest movers.
            let config = Config::load_or_create()?;
            let screen = Screen {
                min_price: config.exploration.min_price,
                min_dollar_volume: config.exploration.min_avg_dollar_volume,
                most_active: usize::MAX,
                movers: 0,
            };
            let stocks = runtime.block_on(fetch_screened_pool(&alpaca, &cache, &screen))?;
            let description = format!(
                "every liquid stock among {} listings (price >= ${:.0}, 20-day dollar volume \
                 >= ${:.0}M)",
                stocks.len(),
                screen.min_price,
                screen.min_dollar_volume / 1e6,
            );
            (stocks, Some(screen), description)
        }
        other => bail!("unknown universe {other:?}; use large, screened, or liquid"),
    };
    let fundamentals = if use_fundamentals {
        let config = Config::load_or_create()?;
        let edgar = crate::market::edgar::EdgarClient::new(&config.edgar.contact)?;
        Some(runtime.block_on(crate::fundamentals::series_from_frames(
            &edgar,
            Utc::now().date_naive(),
        ))?)
    } else {
        None
    };
    let set = match &fundamentals {
        Some(f) => IndicatorSet::growth(f),
        None => IndicatorSet::price(),
    };
    let observations = observe_all(&benchmark, &stocks, horizon, screen.as_ref(), &set);
    if observations.is_empty() {
        bail!("no observations; is there enough history?");
    }
    print!("{}", report(&observations, &description, horizon, &set));
    Ok(())
}

/// Exploration's screen, rebuilt from one day's bars.
#[derive(Debug, Clone)]
pub(crate) struct Screen {
    pub(crate) min_price: f64,
    pub(crate) min_dollar_volume: f64,
    /// Names taken by that day's share volume, as Alpaca's most-actives.
    pub(crate) most_active: usize,
    /// Names taken from each end of that day's percent change.
    pub(crate) movers: usize,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            min_price: 5.0,
            min_dollar_volume: 20_000_000.0,
            most_active: 100,
            movers: 50,
        }
    }
}

/// One stock as the screen sees it on a day.
struct ScreenRow {
    stock: usize,
    bar: usize,
    volume: u64,
    change: f64,
}

impl Screen {
    /// The (stock, bar index) pairs the screen picks on `day`.
    pub(crate) fn select(&self, stocks: &[Series], day: NaiveDate) -> Vec<(usize, usize)> {
        let mut rows: Vec<ScreenRow> = stocks
            .iter()
            .enumerate()
            .filter_map(|(stock, series)| {
                let bars = &series.bars;
                let j = bars.binary_search_by_key(&day, date).ok()?;
                let last_20 = bars.get(j.checked_sub(19)?..=j)?;
                let dollar_volume = last_20
                    .iter()
                    .map(|b| b.close * b.volume as f64)
                    .sum::<f64>()
                    / 20.0;
                let (bar, prev) = (&bars[j], &bars[j - 1]);
                (bar.close >= self.min_price && dollar_volume >= self.min_dollar_volume).then(
                    || ScreenRow {
                        stock,
                        bar: j,
                        volume: bar.volume,
                        change: bar.close / prev.close - 1.0,
                    },
                )
            })
            .collect();

        let mut picked = HashSet::new();
        rows.sort_by(|a, b| b.volume.cmp(&a.volume));
        picked.extend(rows.iter().take(self.most_active).map(|r| (r.stock, r.bar)));
        rows.sort_by(|a, b| b.change.total_cmp(&a.change));
        picked.extend(rows.iter().take(self.movers).map(|r| (r.stock, r.bar)));
        picked.extend(
            rows.iter()
                .rev()
                .take(self.movers)
                .map(|r| (r.stock, r.bar)),
        );
        let mut picked: Vec<_> = picked.into_iter().collect();
        picked.sort();
        picked
    }
}

/// Listings exploration could pick: common stocks and ADRs on the major
/// exchanges, not funds, warrants, units, rights, notes, or preferreds.
pub(crate) fn is_screenable(asset: &crate::market::alpaca::Asset) -> bool {
    const EXCLUDED: [&str; 16] = [
        "ETF",
        "Fund",
        "Warrant",
        " Unit",
        "Right",
        "Notes",
        "Preferred",
        "%",
        "ETN",
        "Trust",
        // Leveraged and inverse fund families, whose names often say neither.
        "ProShares",
        "Direxion",
        "Leveraged",
        "Inverse",
        " 2X",
        " 3X",
    ];
    asset.tradable
        && matches!(asset.exchange.as_str(), "NYSE" | "NASDAQ" | "AMEX")
        && !EXCLUDED.iter().any(|word| asset.name.contains(word))
}

/// Daily bars for every listing that was ever liquid enough to pass the
/// screen. A monthly-bar pass narrows the listings first, so daily history
/// is fetched only for names that could matter.
pub(crate) async fn fetch_screened_pool(
    alpaca: &AlpacaClient,
    cache: &BarCache,
    screen: &Screen,
) -> Result<Vec<Series>> {
    let pool_path = cache.dir.join("screened_pool.json");
    let pool: Vec<String> = match fs::read(&pool_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(pool) => pool,
        None => {
            let listings: Vec<String> = alpaca
                .us_equities()
                .await?
                .iter()
                .filter(|a| is_screenable(a))
                .map(|a| a.symbol.clone())
                .collect();
            eprintln!("{} listings; checking monthly liquidity", listings.len());
            let start = HISTORY_START.parse()?;
            let mut pool = Vec::new();
            for batch in listings.chunks(BATCH) {
                let monthly = alpaca
                    .sip_bars_many(batch, Timeframe::Months(1), start)
                    .await?;
                // A month of about 21 sessions at three quarters of the daily
                // floor: generous, since the daily screen decides.
                let floor = screen.min_dollar_volume * 21.0 * 0.75;
                pool.extend(monthly.into_iter().filter_map(|(symbol, bars)| {
                    bars.iter()
                        .any(|b| b.close >= screen.min_price && b.close * b.volume as f64 >= floor)
                        .then_some(symbol)
                }));
            }
            pool.sort();
            fs::write(&pool_path, serde_json::to_vec(&pool)?)?;
            pool
        }
    };
    eprintln!(
        "{} listings were ever liquid enough; fetching daily bars",
        pool.len()
    );
    let bars = cache.history_many(alpaca, &pool).await?;
    let mut stocks: Vec<Series> = bars
        .into_iter()
        .filter(|(_, bars)| !bars.is_empty())
        .map(|(symbol, bars)| Series { symbol, bars })
        .collect();
    stocks.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    Ok(stocks)
}

/// Symbols per multi-symbol bars request.
const BATCH: usize = 100;

/// Daily bars for each symbol. Symbols that fail are skipped.
async fn fetch(alpaca: &AlpacaClient, cache: &BarCache, symbols: Vec<String>) -> Vec<Series> {
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
    stocks
}

/// Today's daily history per symbol, kept on disk so reruns with other
/// settings don't refetch it. Earlier days' caches are deleted.
pub(crate) struct BarCache {
    dir: PathBuf,
    /// Where each symbol's history starts.
    start: DateTime<Utc>,
}

impl BarCache {
    /// The full history since 2016, for backtests.
    pub(crate) fn today() -> Result<Self> {
        Self::named("bars", HISTORY_START.parse()?)
    }

    /// Today's cache under `name`, with histories from `start`.
    pub(crate) fn named(name: &str, start: DateTime<Utc>) -> Result<Self> {
        let root = dirs::cache_dir()
            .context("no cache directory on this platform")?
            .join("warren")
            .join(name);
        let dir = root.join(Utc::now().format("%Y-%m-%d").to_string());
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for entry in fs::read_dir(&root)?.flatten() {
            if entry.path() != dir {
                fs::remove_dir_all(entry.path()).ok();
            }
        }
        Ok(Self { dir, start })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, symbol: &str) -> PathBuf {
        // Class shares like BRK.B keep their dot; `/` can't be in a file name.
        self.dir.join(format!("{}.json", symbol.replace('/', "_")))
    }

    fn cached(&self, symbol: &str) -> Option<Vec<Bar>> {
        serde_json::from_slice(&fs::read(self.path(symbol)).ok()?).ok()
    }

    fn store(&self, symbol: &str, bars: &[Bar]) -> Result<()> {
        let path = self.path(symbol);
        fs::write(&path, serde_json::to_vec(bars)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    async fn history(&self, alpaca: &AlpacaClient, symbol: &str) -> Result<Vec<Bar>> {
        if let Some(bars) = self.cached(symbol) {
            return Ok(bars);
        }
        let bars = alpaca.daily_history(symbol, self.start).await?;
        self.store(symbol, &bars)?;
        Ok(bars)
    }

    /// Like [`Self::history`] for many symbols, fetching the uncached ones
    /// in batches. Symbols without bars map to an empty history.
    pub(crate) async fn history_many(
        &self,
        alpaca: &AlpacaClient,
        symbols: &[String],
    ) -> Result<HashMap<String, Vec<Bar>>> {
        let mut out = HashMap::new();
        let mut missing = Vec::new();
        for symbol in symbols {
            match self.cached(symbol) {
                Some(bars) => {
                    out.insert(symbol.clone(), bars);
                }
                None => missing.push(symbol.clone()),
            }
        }
        let start = self.start;
        for (i, batch) in missing.chunks(BATCH).enumerate() {
            eprintln!(
                "  daily bars {}/{} symbols",
                (i * BATCH + batch.len()),
                missing.len()
            );
            let mut fetched = alpaca.sip_bars_many(batch, Timeframe::Day, start).await?;
            for symbol in batch {
                let bars = fetched.remove(symbol).unwrap_or_default();
                self.store(symbol, &bars)?;
                out.insert(symbol.clone(), bars);
            }
        }
        Ok(out)
    }
}

pub(crate) struct Series {
    pub(crate) symbol: String,
    pub(crate) bars: Vec<Bar>,
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
    /// Raw indicator values for the run's [`IndicatorSet`], and their
    /// quintiles on the date (0 when unavailable); both are set once every
    /// symbol is observed.
    indicators: Vec<f64>,
    quintiles: Vec<u8>,
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

/// Sample on every `horizon`th benchmark trading day: every symbol, or
/// only what `screen` picks that day.
fn observe_all(
    benchmark: &[Bar],
    stocks: &[Series],
    horizon: usize,
    screen: Option<&Screen>,
    set: &IndicatorSet,
) -> Vec<Observation> {
    let mut observations = Vec::new();
    let mut i = 0;
    while i + horizon < benchmark.len() {
        let day = date(&benchmark[i]);
        let bench_now = window(benchmark, i).and_then(technicals::compute);
        let bench_forward = benchmark[i + horizon].close / benchmark[i + 1].open - 1.0;
        let first = observations.len();
        let sampled: Vec<(usize, usize)> = match screen {
            Some(screen) => screen.select(stocks, day),
            None => stocks
                .iter()
                .enumerate()
                .filter_map(|(k, s)| Some((k, s.bars.binary_search_by_key(&day, date).ok()?)))
                .collect(),
        };
        for (k, j) in sampled {
            let bars = &stocks[k].bars;
            if let Some(mut obs) = observe(bars, j, horizon, bench_now.as_ref(), bench_forward) {
                obs.indicators = set.raw(&stocks[k].symbol, bars, j);
                obs.quintiles = vec![0; set.list.len()];
                observations.push(obs);
            }
        }
        let today = &mut observations[first..];
        if !today.is_empty() {
            let mean = today.iter().map(|o| o.forward).sum::<f64>() / today.len() as f64;
            for obs in today.iter_mut() {
                obs.excess = obs.forward - mean;
            }
            for k in 0..set.list.len() {
                if k == set.composite {
                    for obs in today.iter_mut() {
                        obs.indicators[k] = set.combine(obs);
                    }
                }
                let values: Vec<f64> = today.iter().map(|o| o.indicators[k]).collect();
                for (obs, q) in today.iter_mut().zip(quintiles(&values)) {
                    obs.quintiles[k] = q;
                }
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
        indicators: Vec::new(),
        quintiles: Vec::new(),
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

fn report(
    observations: &[Observation],
    universe: &str,
    horizon: usize,
    set: &IndicatorSet,
) -> String {
    let dates: Vec<NaiveDate> = {
        let mut d: Vec<_> = observations.iter().map(|o| o.date).collect();
        d.dedup();
        d
    };
    let midpoint = dates[dates.len() / 2];
    let mut out = format!(
        "Quant factor backtest: {universe}\n{} to {}, {} dates, {} observations.\n\
         Held {horizon} trading days from the next open. \"vs universe\" is the mean \
         return above the\nequal-weight average of the names sampled on the same date; \
         \"beat it\" is how often. Spreads are per-date\nmeans of high (4–5) minus low (1–2) scores, with \
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
        out += &format!(
            "  spread     {}\n",
            halves_spread(
                observations,
                midpoint,
                |o| o.scores[k] >= 4,
                |o| o.scores[k] <= 2
            )
        );
    }

    out += "\ncandidate indicators (quintiles across the names sampled each date; \
            spread is Q5 minus Q1)\n";
    for (k, indicator) in set.list.iter().enumerate() {
        out += &format!(
            "\n{} — expect {}\n{BUCKET_HEADER}",
            indicator.name, indicator.expect
        );
        for q in 1..=5u8 {
            let group: Vec<_> = observations
                .iter()
                .filter(|o| o.quintiles[k] == q)
                .collect();
            if !group.is_empty() {
                out += &bucket_row(&format!("Q{q}"), &group);
            }
        }
        out += &format!(
            "  spread     {}\n",
            halves_spread(
                observations,
                midpoint,
                |o| o.quintiles[k] == 5,
                |o| o.quintiles[k] == 1
            )
        );
        if let Some((q1, q5)) = typical_cutoffs(observations, k) {
            out +=
                &format!("  cutoffs    Q1 below ~{q1:.3}, Q5 above ~{q5:.3} (median over dates)\n");
        }
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
    {
        let mut trades: Vec<f64> = Vec::new();
        let returns: Vec<f64> = periods
            .iter()
            .map(|(_, obs)| {
                let picked: Vec<f64> = obs
                    .iter()
                    .filter(|o| o.quintiles[set.composite] == 5)
                    .map(|o| o.held)
                    .collect();
                trades.extend(&picked);
                mean(picked).unwrap_or(0.0)
            })
            .collect();
        let wins = trades.iter().filter(|&&t| t > 0.0).count();
        out += &format!(
            "{:<17} {} {:>7.0}% {:>7.1} {:>8} {:>8.0}%\n",
            "  hold composite5",
            curve_stats(&returns, years, horizon),
            100.0,
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

/// The values where indicator `k`'s bottom and top quintiles typically
/// begin: the 20th and 80th percentiles on each date, medianed over dates.
/// These turn a cross-sectional ranking into fixed thresholds live code can
/// apply to one stock at a time.
fn typical_cutoffs(observations: &[Observation], k: usize) -> Option<(f64, f64)> {
    let mut low = Vec::new();
    let mut high = Vec::new();
    for (_, obs) in by_period(observations) {
        let mut values: Vec<f64> = obs
            .iter()
            .map(|o| o.indicators[k])
            .filter(|v| v.is_finite())
            .collect();
        if values.len() < 5 {
            continue;
        }
        values.sort_by(f64::total_cmp);
        low.push(values[values.len() / 5]);
        high.push(values[values.len() * 4 / 5]);
    }
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied()
    };
    Some((median(low)?, median(high)?))
}

/// `spread` over all dates and each half of them.
fn halves_spread(
    observations: &[Observation],
    midpoint: NaiveDate,
    high: impl Fn(&Observation) -> bool,
    low: impl Fn(&Observation) -> bool,
) -> String {
    [
        ("all", None),
        ("first half", Some(true)),
        ("second half", Some(false)),
    ]
    .iter()
    .filter_map(|(label, first)| {
        let in_half = |o: &Observation| first.is_none_or(|f| (o.date < midpoint) == f);
        let (m, t) = spread(
            observations,
            |o| in_half(o) && high(o),
            |o| in_half(o) && low(o),
        )?;
        Some(format!("{label} {} (t {t:.1})", pct(m)))
    })
    .collect::<Vec<_>>()
    .join(", ")
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
    fn quintiles_split_finite_values_evenly() {
        let values = [5.0, 1.0, f64::NAN, 3.0, 2.0, 4.0, 10.0, 9.0, 8.0, 7.0, 6.0];
        let q = quintiles(&values);
        assert_eq!(q[2], 0);
        assert_eq!(q[1], 1); // lowest
        assert_eq!(q[6], 5); // highest
        for bucket in 1..=5 {
            assert_eq!(q.iter().filter(|&&x| x == bucket).count(), 2);
        }
    }

    #[test]
    fn indicators_use_only_past_bars() {
        let bars = series(800);
        let before = price_indicators(&bars, 700);
        let mut altered = bars.clone();
        for b in &mut altered[701..] {
            b.close *= 2.0;
            b.volume *= 2;
        }
        // The composite is filled in later, across names.
        assert_eq!(
            before[..COMPOSITE],
            price_indicators(&altered, 700)[..COMPOSITE]
        );
        assert!(before[..COMPOSITE].iter().all(|v| v.is_finite()));
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
        let observations = observe_all(
            &bench,
            &[a, b],
            DEFAULT_HORIZON,
            None,
            &IndicatorSet::price(),
        );
        assert!(!observations.is_empty());
        for (_, obs) in by_period(&observations) {
            let total: f64 = obs.iter().map(|o| o.excess).sum();
            assert!(total.abs() < 1e-12);
        }
    }

    #[test]
    fn screen_picks_liquid_actives_and_movers_that_day() {
        // Five liquid names with distinct volumes and moves, and one too
        // cheap to qualify.
        let make = |symbol: &str, price: f64, volume: u64, last_change: f64| {
            let mut bars: Vec<Bar> = (0..30)
                .map(|d| bar(d, price, price, price, price))
                .collect();
            for b in &mut bars {
                b.volume = volume;
            }
            let last = bars.last_mut().unwrap();
            last.close = price * (1.0 + last_change);
            Series {
                symbol: symbol.into(),
                bars,
            }
        };
        let stocks = vec![
            make("BIG", 50.0, 9_000_000, 0.00),
            make("UP", 50.0, 1_000_000, 0.20),
            make("DOWN", 50.0, 1_000_000, -0.15),
            make("FLAT1", 50.0, 1_000_000, 0.01),
            make("FLAT2", 50.0, 1_000_000, -0.01),
            make("PENNY", 2.0, 90_000_000, 0.50),
        ];
        let screen = Screen {
            min_price: 5.0,
            min_dollar_volume: 20_000_000.0,
            most_active: 1,
            movers: 1,
        };
        let day = date(&stocks[0].bars[29]);
        let picked: Vec<&str> = screen
            .select(&stocks, day)
            .iter()
            .map(|&(k, _)| stocks[k].symbol.as_str())
            .collect();
        assert_eq!(picked, ["BIG", "UP", "DOWN"]);
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
