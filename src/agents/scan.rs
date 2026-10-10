//! A ranked scan of every liquid US stock, so exploration fishes from the
//! whole market instead of only the day's most active names and biggest
//! movers.
//!
//! The rank is the composite that held up in backtests over every liquid
//! stock, 2017–2026: the 12-1 month momentum quintile plus the 52-week-high
//! quintile plus the extension score (3–15). Held for 21 trading days, its
//! top fifth beat the average liquid stock by about 2% a year, though not
//! SPY; see `backtest --universe liquid`.
//!
//! Bars are cached for the day, and the scan with them, so every agent
//! shares one pull.

use std::{collections::HashMap, fs};

use anyhow::{Result, bail};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{AgentCtx, scoring, technicals};
use crate::backtest::{BarCache, MIN_GROWTH_GROSS_MARGIN, is_screenable, quintiles};
use crate::market::alpaca::Timeframe;

/// Symbols per multi-symbol request.
const BATCH: usize = 100;
/// Enough daily history for 12-1 momentum and the 52-week high.
const HISTORY_DAYS: i64 = 400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRow {
    pub symbol: String,
    pub price: f64,
    /// 3 (worst) to 15 (best).
    pub composite: u8,
    pub return_12_1_percent: f64,
    pub percent_below_52w_high: f64,
    /// 1 (stretched) to 5 (calm).
    pub extension: u8,
    pub return_1m_percent: Option<f64>,
    pub avg_dollar_volume_20d: f64,
}

/// Every liquid stock above the floors, best composite first.
pub async fn scan(ctx: &AgentCtx, min_price: f64, min_dollar_volume: f64) -> Result<Vec<ScanRow>> {
    if ctx.as_of.is_some() {
        bail!("the universe scan only works on current data, not in replays");
    }
    let cache = BarCache::named("scan", Utc::now() - Duration::days(HISTORY_DAYS))?;
    let path = cache
        .dir()
        .join(format!("scan-{min_price}-{min_dollar_volume}.json"));
    if let Some(rows) = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        return Ok(rows);
    }
    let alpaca = &ctx.alpaca;

    // Which listings are liquid now, from a month of bars.
    let listings: Vec<String> = alpaca
        .us_equities()
        .await?
        .iter()
        .filter(|a| is_screenable(a))
        .map(|a| a.symbol.clone())
        .collect();
    let mut liquid = Vec::new();
    let month_ago = Utc::now() - Duration::days(30);
    for batch in listings.chunks(BATCH) {
        let recent = alpaca
            .sip_bars_many(batch, Timeframe::Day, month_ago)
            .await?;
        for (symbol, bars) in recent {
            let Some(last) = bars.last() else { continue };
            let dollar_volume =
                bars.iter().map(|b| b.close * b.volume as f64).sum::<f64>() / bars.len() as f64;
            if last.close >= min_price && dollar_volume >= min_dollar_volume {
                liquid.push(symbol);
            }
        }
    }
    liquid.sort();

    // Indicators from a year of history.
    let history = cache.history_many(alpaca, &liquid).await?;
    struct Raw {
        symbol: String,
        price: f64,
        near_high: f64,
        momentum: f64,
        extension: u8,
        return_1m: Option<f64>,
        dollar_volume: f64,
    }
    let raw: Vec<Raw> = history
        .iter()
        .filter_map(|(symbol, bars)| {
            let t = technicals::compute(bars)?;
            let extension = scoring::quant_factors(&t, None).extension?.score;
            Some(Raw {
                symbol: symbol.clone(),
                price: t.last_close,
                near_high: 1.0 - t.percent_below_52w_high / 100.0,
                momentum: t.return_12_1_percent?,
                extension,
                return_1m: t.return_1m_percent,
                dollar_volume: t.avg_dollar_volume_20d?,
            })
        })
        .collect();

    let high_q = quintiles(&raw.iter().map(|r| r.near_high).collect::<Vec<_>>());
    let momentum_q = quintiles(&raw.iter().map(|r| r.momentum).collect::<Vec<_>>());
    let mut rows: Vec<ScanRow> = raw
        .iter()
        .zip(high_q.iter().zip(&momentum_q))
        .map(|(r, (h, m))| ScanRow {
            symbol: r.symbol.clone(),
            price: r.price,
            composite: h + m + r.extension,
            return_12_1_percent: r.momentum,
            percent_below_52w_high: (1.0 - r.near_high) * 100.0,
            extension: r.extension,
            return_1m_percent: r.return_1m,
            avg_dollar_volume_20d: r.dollar_volume,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.composite
            .cmp(&a.composite)
            .then(b.return_12_1_percent.total_cmp(&a.return_12_1_percent))
    });
    fs::write(&path, serde_json::to_vec(&rows)?)?;
    Ok(rows)
}

/// A row of the growth ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrowthRow {
    pub symbol: String,
    pub price: f64,
    /// 4 (worst) to 20 (best): revenue growth, acceleration, cheapness
    /// (price/sales, reversed), and 12-1 momentum quintiles.
    pub composite: u8,
    pub revenue_growth_percent: f64,
    pub acceleration_points: f64,
    pub price_to_sales: f64,
    pub market_cap: Option<f64>,
    pub return_12_1_percent: f64,
    pub gross_margin_percent: Option<f64>,
}

/// Liquid stocks with US-GAAP fundamentals and gross margins of 40% or
/// more, ranked by the growth composite. Over 2017–2026 its top fifth, held
/// 3–6 months, beat the average liquid stock by about 1.5–2% a year but
/// trailed SPY by about 3% (`backtest --universe liquid --fundamentals`).
/// Without the margin floor it roughly matched SPY, but by ranking
/// thin-margin cyclicals (refiners, distributors) whose revenue rose with
/// commodity prices: value and momentum rather than growth. Fundamentals come from
/// SEC frames and count from 90 days after each quarter, as in the
/// backtest, so the latest quarter can be missing; research reads each
/// company's actual filings.
pub async fn growth_scan(
    ctx: &AgentCtx,
    min_price: f64,
    min_dollar_volume: f64,
) -> Result<Vec<GrowthRow>> {
    let cache = BarCache::named("scan", Utc::now() - Duration::days(HISTORY_DAYS))?;
    let path = cache
        .dir()
        .join(format!("growth-{min_price}-{min_dollar_volume}.json"));
    if let Some(rows) = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        return Ok(rows);
    }
    let rows = scan(ctx, min_price, min_dollar_volume).await?;
    let edgar = crate::market::edgar::EdgarClient::new(&ctx.config.edgar.contact)?;
    let today = Utc::now().date_naive();
    let series = crate::fundamentals::series_from_frames(&edgar, today).await?;

    struct Raw<'a> {
        row: &'a ScanRow,
        growth: f64,
        acceleration: f64,
        price_to_sales: f64,
        market_cap: Option<f64>,
        gross_margin: Option<f64>,
    }
    let raw: Vec<Raw> = rows
        .iter()
        .filter_map(|row| {
            let m = series.get(&row.symbol)?.metrics(today);
            let (market_cap, price_to_sales) = m.valuation(row.price);
            Some(Raw {
                row,
                growth: m.revenue_growth_percent?,
                acceleration: m.acceleration_points?,
                price_to_sales: price_to_sales?,
                market_cap,
                gross_margin: m.gross_margin_percent,
            })
        })
        .collect();
    let q = |f: fn(&Raw) -> f64| quintiles(&raw.iter().map(f).collect::<Vec<_>>());
    let (growth_q, accel_q, ps_q, momentum_q) = (
        q(|r| r.growth),
        q(|r| r.acceleration),
        q(|r| r.price_to_sales),
        q(|r| r.row.return_12_1_percent),
    );
    // Quintiles across every name, as in the backtest, then only businesses
    // with growth-company margins are ranked.
    let mut ranked: Vec<GrowthRow> = raw
        .iter()
        .enumerate()
        .filter(|(_, r)| r.gross_margin.is_some_and(|g| g >= MIN_GROWTH_GROSS_MARGIN))
        .map(|(i, r)| GrowthRow {
            symbol: r.row.symbol.clone(),
            price: r.row.price,
            composite: growth_q[i] + accel_q[i] + (6 - ps_q[i]) + momentum_q[i],
            revenue_growth_percent: r.growth,
            acceleration_points: r.acceleration,
            price_to_sales: r.price_to_sales,
            market_cap: r.market_cap,
            return_12_1_percent: r.row.return_12_1_percent,
            gross_margin_percent: r.gross_margin,
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.composite.cmp(&a.composite).then(
            b.revenue_growth_percent
                .total_cmp(&a.revenue_growth_percent),
        )
    });
    fs::write(&path, serde_json::to_vec(&ranked)?)?;
    Ok(ranked)
}

/// The top `n` rows the agent may buy: not excluded, and within its sectors
/// and foreign-issuer rule when it has them. Returns the rows and their
/// sectors, when known.
pub async fn top_for_agent<T: Clone>(
    ctx: &AgentCtx,
    rows: &[T],
    symbol: fn(&T) -> &str,
    n: usize,
) -> Vec<(T, Option<&'static str>)> {
    let universe = &ctx.strategy.universe;
    let mut out = Vec::new();
    // Look up sectors in chunks, only as far down the ranking as needed.
    for chunk in rows
        .iter()
        .filter(|r| !universe.excludes(symbol(r)))
        .collect::<Vec<_>>()
        .chunks(25)
    {
        let symbols: Vec<String> = chunk.iter().map(|r| symbol(r).to_string()).collect();
        let profiles: HashMap<String, crate::sectors::CompanyProfile> = match &ctx.sectors {
            Some(directory) => directory.profiles(&symbols).await.unwrap_or_default(),
            None => HashMap::new(),
        };
        let restricted = !universe.sectors.is_empty() || !universe.allow_foreign;
        for row in chunk {
            let profile = profiles.get(symbol(row));
            let allowed = match profile {
                Some(p) => p.exclusion(universe).is_none(),
                None => !restricted,
            };
            if allowed {
                out.push((
                    (*row).clone(),
                    profile.and_then(|p| p.sector).map(|s| s.label()),
                ));
                if out.len() == n {
                    return out;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scans the live market; run with
    /// `cargo test live_scan --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_scan() {
        let (ctx, dir) = crate::agents::test_support::live_ctx();
        let rows = crate::runtime::RUNTIME
            .block_on(scan(&ctx, 5.0, 20_000_000.0))
            .unwrap();
        println!("{} liquid stocks ranked", rows.len());
        for r in rows.iter().take(15) {
            println!(
                "{:6} {:>9.2} comp {:2} 12-1 {:+6.1}% below high {:5.1}% ext {} 1m {:+.1}%",
                r.symbol,
                r.price,
                r.composite,
                r.return_12_1_percent,
                r.percent_below_52w_high,
                r.extension,
                r.return_1m_percent.unwrap_or(f64::NAN)
            );
        }
        assert!(rows.len() > 500);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Ranks the live market for growth; run with
    /// `cargo test live_growth_scan --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_growth_scan() {
        let (mut ctx, dir) = crate::agents::test_support::live_ctx();
        ctx.config = std::sync::Arc::new(crate::config::Config::load_or_create().unwrap());
        let rows = crate::runtime::RUNTIME
            .block_on(growth_scan(&ctx, 5.0, 20_000_000.0))
            .unwrap();
        println!("{} stocks ranked for growth", rows.len());
        for r in rows.iter().take(15) {
            println!(
                "{:6} {:>9.2} comp {:2} growth {:+6.1}% accel {:+5.1} p/s {:5.1} cap {:>7.1}B 12-1 {:+6.1}%",
                r.symbol,
                r.price,
                r.composite,
                r.revenue_growth_percent,
                r.acceleration_points,
                r.price_to_sales,
                r.market_cap.unwrap_or(0.0) / 1e9,
                r.return_12_1_percent,
            );
        }
        assert!(rows.len() > 300);
        std::fs::remove_dir_all(dir).ok();
    }
}
