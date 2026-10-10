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
use crate::backtest::{BarCache, is_screenable, quintiles};
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

/// The top `n` rows the agent may buy: not excluded, and within its sectors
/// and foreign-issuer rule when it has them. Returns the rows and their
/// sectors, when known.
pub async fn top_for_agent(
    ctx: &AgentCtx,
    rows: &[ScanRow],
    n: usize,
) -> Vec<(ScanRow, Option<&'static str>)> {
    let universe = &ctx.strategy.universe;
    let mut out = Vec::new();
    // Look up sectors in chunks, only as far down the ranking as needed.
    for chunk in rows
        .iter()
        .filter(|r| !universe.excludes(&r.symbol))
        .collect::<Vec<_>>()
        .chunks(25)
    {
        let symbols: Vec<String> = chunk.iter().map(|r| r.symbol.clone()).collect();
        let profiles: HashMap<String, crate::sectors::CompanyProfile> = match &ctx.sectors {
            Some(directory) => directory.profiles(&symbols).await.unwrap_or_default(),
            None => HashMap::new(),
        };
        let restricted = !universe.sectors.is_empty() || !universe.allow_foreign;
        for row in chunk {
            let profile = profiles.get(&row.symbol);
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
}
