//! Tools the agents call. Each wraps the Alpaca client or the store and
//! returns compact JSON. Failures go back to the model as tool errors it can
//! read and react to, rather than aborting the run.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, Local, Utc};
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use rig_core::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentCtx, book::Book, executor, scan, sweep, technicals};
use crate::market::alpaca::{NewOrder, OrderSide, Timeframe};
use crate::store::JournalEntry;

/// Benchmark for judging past decisions.
pub const BENCHMARK: &str = "SPY";

type ToolResult = Result<Value, ToolExecutionError>;

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("tool schema serializes")
}

fn failed(err: anyhow::Error) -> ToolExecutionError {
    ToolExecutionError::other(format!("{err:#}"))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn normalize(symbols: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    symbols
        .iter()
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

/// Declares a tool struct holding the agent context and implements [`Tool`]
/// for it by delegating to an async `run` method.
macro_rules! tool {
    ($ty:ident, $name:literal, $args:ty, $description:expr) => {
        #[derive(Clone)]
        pub struct $ty(pub AgentCtx);

        impl Tool for $ty {
            const NAME: &'static str = $name;
            type Args = $args;
            type Output = Value;
            type Error = ToolExecutionError;

            fn description(&self) -> String {
                $description.to_string()
            }

            fn parameters(&self) -> Value {
                schema::<$args>()
            }

            async fn call(&self, _: &mut ToolContext, args: Self::Args) -> ToolResult {
                self.run(args).await
            }
        }
    };
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct SymbolArgs {
    /// Ticker symbol, e.g. AAPL.
    pub symbol: String,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct SymbolsArgs {
    /// Ticker symbols, e.g. ["AAPL", "MSFT"]. At most 50.
    pub symbols: Vec<String>,
}

// ---------------------------------------------------------------------------
// Market data
// ---------------------------------------------------------------------------

tool!(
    GetQuotes,
    "get_quotes",
    SymbolsArgs,
    "Latest price, today's change versus the previous close, today's range and volume, \
     and the current bid/ask for up to 50 symbols."
);

impl GetQuotes {
    async fn run(&self, args: SymbolsArgs) -> ToolResult {
        let symbols = normalize(&args.symbols);
        if symbols.is_empty() || symbols.len() > 50 {
            return Err(ToolExecutionError::invalid_args("pass 1 to 50 symbols"));
        }
        if let Some(as_of) = self.0.as_of {
            return quotes_as_of(&self.0, &symbols, as_of).await;
        }
        let snapshots = self.0.alpaca.snapshots(&symbols).await.map_err(failed)?;
        let quotes: Vec<Value> = symbols
            .iter()
            .map(|symbol| match snapshots.get(symbol) {
                None => {
                    json!({ "symbol": symbol, "error": "no data (unknown or untraded symbol)" })
                }
                Some(s) => json!({
                    "symbol": symbol,
                    "price": s.last_price(),
                    "change_percent": s.change_percent().map(round2),
                    "previous_close": s.prev_daily_bar.as_ref().map(|b| b.close),
                    "day_high": s.daily_bar.as_ref().map(|b| b.high),
                    "day_low": s.daily_bar.as_ref().map(|b| b.low),
                    "day_volume": s.daily_bar.as_ref().map(|b| b.volume),
                    "bid": s.latest_quote.as_ref().map(|q| q.bid_price),
                    "ask": s.latest_quote.as_ref().map(|q| q.ask_price),
                    "as_of": s.latest_trade.as_ref().map(|t| t.timestamp),
                }),
            })
            .collect();
        Ok(json!({
            "quotes": quotes,
            "note": "IEX feed: last trade and daily bars are reliable; bid/ask can be thin or stale.",
        }))
    }
}

tool!(
    GetTechnicals,
    "get_technicals",
    SymbolArgs,
    "Technical indicators from about 18 months of daily bars: 20/50/200-day SMAs, \
     14-day RSI and ATR, 1-week to 1-year returns, 3-month annualized volatility, \
     52-week range, and 20-day average dollar volume."
);

impl GetTechnicals {
    async fn run(&self, args: SymbolArgs) -> ToolResult {
        let symbol = args.symbol.trim().to_uppercase();
        let start = self.0.now() - Duration::days(550);
        let bars = self
            .0
            .alpaca
            .daily_history_until(&symbol, start, self.0.as_of)
            .await
            .map_err(failed)?;
        let technicals = technicals::compute(&bars)
            .ok_or_else(|| ToolExecutionError::not_found(format!("no daily bars for {symbol}")))?;
        Ok(json!({ "symbol": symbol, "technicals": technicals }))
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct PriceHistoryArgs {
    pub symbol: String,
    /// "day" for daily bars or "hour" for hourly bars.
    pub interval: String,
    /// How many calendar days back to start. Hourly is limited to 30.
    pub days: u32,
}

/// Bars returned per call; longer histories are thinned evenly.
const MAX_HISTORY_ROWS: usize = 120;

tool!(
    GetPriceHistory,
    "get_price_history",
    PriceHistoryArgs,
    "OHLCV bars for a symbol as compact rows [date, open, high, low, close, volume], \
     oldest first, thinned to at most 120 rows."
);

impl GetPriceHistory {
    async fn run(&self, args: PriceHistoryArgs) -> ToolResult {
        let symbol = args.symbol.trim().to_uppercase();
        let (timeframe, max_days) = match args.interval.as_str() {
            "day" => (Timeframe::Day, 1100),
            "hour" => (Timeframe::Hours(1), 30),
            other => {
                return Err(ToolExecutionError::invalid_args(format!(
                    "interval must be \"day\" or \"hour\", not {other:?}"
                )));
            }
        };
        let days = args.days.clamp(1, max_days);
        let bars = self
            .0
            .alpaca
            .bars_until(
                &symbol,
                timeframe,
                self.0.now() - Duration::days(days.into()),
                self.0.as_of,
            )
            .await
            .map_err(failed)?;
        let step = bars.len().div_ceil(MAX_HISTORY_ROWS).max(1);
        let rows: Vec<Value> = bars
            .iter()
            .rev()
            .step_by(step)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|b| {
                let time = match timeframe {
                    Timeframe::Day => b.timestamp.format("%Y-%m-%d").to_string(),
                    _ => b.timestamp.format("%Y-%m-%d %H:%M").to_string(),
                };
                json!([time, b.open, b.high, b.low, b.close, b.volume])
            })
            .collect();
        Ok(json!({
            "symbol": symbol,
            "interval": args.interval,
            "total_bars": bars.len(),
            "every_nth_bar": step,
            "rows": rows,
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct NewsArgs {
    /// Symbols to fetch news for. At most 20.
    pub symbols: Vec<String>,
    /// How many days back to look, 1 to 30.
    pub days: u32,
    /// Maximum headlines, 1 to 50.
    pub limit: u32,
}

tool!(
    GetNews,
    "get_news",
    NewsArgs,
    "Recent news headlines and summaries for symbols (Benzinga via Alpaca), newest first."
);

impl GetNews {
    async fn run(&self, args: NewsArgs) -> ToolResult {
        let symbols = normalize(&args.symbols);
        if symbols.is_empty() || symbols.len() > 20 {
            return Err(ToolExecutionError::invalid_args("pass 1 to 20 symbols"));
        }
        let start = self.0.now() - Duration::days(args.days.clamp(1, 30).into());
        let news = self
            .0
            .alpaca
            .news(&symbols, start, self.0.as_of, args.limit.clamp(1, 50))
            .await
            .map_err(failed)?;
        Ok(json!({ "articles": news }))
    }
}

tool!(
    GetAsset,
    "get_asset",
    SymbolArgs,
    "Whether a symbol is an active, tradable US equity, its exchange, and whether it is \
     shortable or fractionable. Use this to rule out OTC listings, warrants, and units."
);

impl GetAsset {
    async fn run(&self, args: SymbolArgs) -> ToolResult {
        let symbol = args.symbol.trim().to_uppercase();
        let asset =
            self.0.alpaca.asset(&symbol).await.map_err(|err| {
                ToolExecutionError::not_found(format!("no asset {symbol}: {err:#}"))
            })?;
        Ok(json!(asset))
    }
}

// ---------------------------------------------------------------------------
// Exploration
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct ScanArgs {
    /// How many top-ranked names to return, 1 to 60.
    pub top: u32,
}

tool!(
    ScanUniverse,
    "scan_universe",
    ScanArgs,
    "Every liquid US stock this agent may buy, ranked by a composite that held up in \
     backtests for the agent's style. Swing: 12-1 month momentum and nearness to the \
     52-week high (each a quintile across the market) plus how calm the price is \
     (extension). Growth: revenue growth, growth acceleration, cheapness (price/sales), \
     and 12-1 month momentum, each a quintile. Returns the top names with the figures \
     behind the rank and their sector. The first call of the day can take a minute or two."
);

impl ScanUniverse {
    async fn run(&self, args: ScanArgs) -> ToolResult {
        let ctx = &self.0;
        let universe = &ctx.strategy.universe;
        let n = args.top.clamp(1, 60) as usize;
        if ctx.strategy.style == crate::strategy::Style::Growth {
            let rows = scan::growth_scan(ctx, universe.min_price, universe.min_avg_dollar_volume)
                .await
                .map_err(failed)?;
            let total = rows.len();
            let top = scan::top_for_agent(ctx, &rows, |r| r.symbol.as_str(), n).await;
            let ranked: Vec<Value> = top
                .iter()
                .map(|(r, sector)| {
                    json!({
                        "symbol": r.symbol,
                        "price": round2(r.price),
                        "composite": r.composite,
                        "revenue_growth_percent": round2(r.revenue_growth_percent),
                        "acceleration_points": round2(r.acceleration_points),
                        "price_to_sales": round2(r.price_to_sales),
                        "market_cap": r.market_cap.map(f64::round),
                        "gross_margin_percent": r.gross_margin_percent.map(round2),
                        "return_12_1_percent": round2(r.return_12_1_percent),
                        "sector": sector,
                    })
                })
                .collect();
            return Ok(json!({
                "ranking": "growth: revenue growth + acceleration + cheapness (price/sales) + \
                            12-1 momentum, each a quintile; composite 4 to 20",
                "stocks_ranked": total,
                "top": ranked,
            }));
        }
        let rows = scan::scan(ctx, universe.min_price, universe.min_avg_dollar_volume)
            .await
            .map_err(failed)?;
        let total = rows.len();
        let top = scan::top_for_agent(ctx, &rows, |r| r.symbol.as_str(), n).await;
        let ranked: Vec<Value> = top
            .iter()
            .map(|(r, sector)| {
                json!({
                    "symbol": r.symbol,
                    "price": round2(r.price),
                    "composite": r.composite,
                    "return_12_1_percent": round2(r.return_12_1_percent),
                    "percent_below_52w_high": round2(r.percent_below_52w_high),
                    "extension": r.extension,
                    "return_1m_percent": r.return_1m_percent.map(round2),
                    "avg_dollar_volume_20d": r.avg_dollar_volume_20d.round(),
                    "sector": sector,
                })
            })
            .collect();
        Ok(json!({
            "ranking": "swing: 12-1 momentum + near the 52-week high (quintiles) + extension; \
                        composite 3 to 15",
            "stocks_ranked": total,
            "top": ranked,
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct ScreenArgs {
    /// How many names to request from the screener, 1 to 50.
    pub top: u32,
}

tool!(
    ScreenMarket,
    "screen_market",
    ScreenArgs,
    "Today's most active stocks and biggest gainers and losers, with prices. Names below \
     the configured minimum price are already filtered out; screen the rest yourself."
);

impl ScreenMarket {
    async fn run(&self, args: ScreenArgs) -> ToolResult {
        let top = args.top.clamp(1, 50);
        let alpaca = &self.0.alpaca;
        let min_price = self.0.config.exploration.min_price;
        let universe = &self.0.strategy.universe;
        let (actives, movers) =
            tokio::try_join!(alpaca.most_actives(top), alpaca.movers(top)).map_err(failed)?;

        // The most-actives screener has no prices; look them up.
        let active_symbols: Vec<String> = actives.iter().map(|a| a.symbol.clone()).collect();
        let prices = alpaca.snapshots(&active_symbols).await.map_err(failed)?;
        let actives: Vec<Value> = actives
            .iter()
            .filter_map(|a| {
                if universe.excludes(&a.symbol) {
                    return None;
                }
                let snapshot = prices.get(&a.symbol)?;
                let price = snapshot.last_price()?;
                (price >= min_price).then(|| {
                    json!({
                        "symbol": a.symbol,
                        "price": price,
                        "change_percent": snapshot.change_percent().map(round2),
                        "volume": a.volume,
                        "dollar_volume": round2(price * a.volume),
                    })
                })
            })
            .collect();
        let movers_above = |list: &[crate::market::alpaca::Mover]| -> Vec<Value> {
            list.iter()
                .filter(|m| m.price >= min_price && !universe.excludes(&m.symbol))
                .map(|m| json!({ "symbol": m.symbol, "price": m.price, "change_percent": round2(m.percent_change) }))
                .collect()
        };
        let mut lists = [
            actives,
            movers_above(&movers.gainers),
            movers_above(&movers.losers),
        ];
        let note = label_sectors(&self.0, &mut lists).await;
        let [most_active, gainers, losers] = lists;
        Ok(json!({
            "min_price_filter": min_price,
            "most_active": most_active,
            "gainers": gainers,
            "losers": losers,
            "note": note,
        }))
    }
}

/// Add each row's sector and drop rows outside the agent's universe.
/// Returns a note on how the rows were filtered.
async fn label_sectors(ctx: &AgentCtx, lists: &mut [Vec<Value>]) -> String {
    let universe = &ctx.strategy.universe;
    let restricted = !universe.sectors.is_empty() || !universe.allow_foreign;
    let Some(directory) = &ctx.sectors else {
        return if restricted {
            "Sector data is off, so these are NOT filtered to this agent's sectors; buys \
             outside them will be refused."
                .into()
        } else {
            "Sector data is off.".into()
        };
    };
    let mut symbols: Vec<String> = lists
        .iter()
        .flatten()
        .filter_map(|row| Some(row["symbol"].as_str()?.to_string()))
        .collect();
    symbols.sort();
    symbols.dedup();
    let profiles = match directory.profiles(&symbols).await {
        Ok(profiles) => profiles,
        Err(err) => return format!("Sector lookup failed ({err:#}); rows are unfiltered."),
    };
    let mut dropped = 0;
    for list in lists.iter_mut() {
        list.retain_mut(|row| {
            let Some(profile) = row["symbol"].as_str().and_then(|s| profiles.get(s)) else {
                return !restricted;
            };
            if profile.exclusion(universe).is_some() {
                dropped += 1;
                return false;
            }
            row["sector"] = json!(profile.sector.map(|s| s.label()));
            row["foreign"] = json!(profile.foreign);
            true
        });
    }
    if restricted {
        format!("Filtered to this agent's universe; {dropped} names outside it were removed.")
    } else {
        "Sectors are from SEC industry codes.".into()
    }
}

// ---------------------------------------------------------------------------
// Research and portfolio state
// ---------------------------------------------------------------------------

tool!(
    GetResearchReport,
    "get_research_report",
    SymbolArgs,
    "The latest full research report for a symbol, including when it was written: \
     thesis, risks, dated news highlights, levels, factor scores, and trend health. All \
     of it is evidence for your decision, not a verdict."
);

impl GetResearchReport {
    async fn run(&self, args: SymbolArgs) -> ToolResult {
        let symbol = args.symbol.trim().to_uppercase();
        match self.0.store.report(&symbol).map_err(failed)? {
            Some(report) => Ok(json!({
                "age_hours": (Utc::now() - report.generated_at).num_hours(),
                "report": report,
            })),
            None => Err(ToolExecutionError::not_found(format!(
                "no research report for {symbol}"
            ))),
        }
    }
}

tool!(
    ListResearch,
    "list_research",
    NoArgs,
    "One line per researched symbol: the thesis, entry range, target, stop, \
     reward-to-risk, factor scores (1 = very bearish, 5 = very bullish; context, not \
     a verdict), the most serious risk, trend health, and report age. Read the full \
     report with get_research_report before acting."
);

impl ListResearch {
    async fn run(&self, _: NoArgs) -> ToolResult {
        let reports = self.0.store.reports().map_err(failed)?;
        let rows: Vec<Value> = reports
            .iter()
            .map(|r| {
                json!({
                    "symbol": r.symbol,
                    "thesis": r.findings.thesis,
                    "price_at_research": r.price_at_research,
                    "entry_low": r.findings.entry_low,
                    "entry_high": r.findings.entry_high,
                    "target_price": r.findings.target_price,
                    "stop_loss": r.findings.stop_loss,
                    "risk_reward": r.risk_reward.map(round2),
                    "horizon_days": r.findings.horizon_days,
                    "factors": r
                        .factors
                        .named()
                        .into_iter()
                        .map(|(name, f)| (name.to_string(), json!(f.score)))
                        .collect::<serde_json::Map<_, _>>(),
                    "top_risk": r.findings.risks.first(),
                    "revenue_growth_percent": r
                        .fundamentals
                        .and_then(|f| f.metrics.revenue_growth_percent)
                        .map(round2),
                    "price_to_sales": r.fundamentals.and_then(|f| f.price_to_sales).map(round2),
                    "trend_health": r.trend_health,
                    "age_hours": (Utc::now() - r.generated_at).num_hours(),
                })
            })
            .collect();
        Ok(json!({ "reports": rows }))
    }
}

tool!(
    GetPortfolio,
    "get_portfolio",
    NoArgs,
    "The paper account (equity, cash, buying power, today's P/L), every open position with \
     cost basis and unrealized P/L, and all open orders."
);

impl GetPortfolio {
    async fn run(&self, _: NoArgs) -> ToolResult {
        let ctx = &self.0;
        let (book, clock) =
            tokio::try_join!(Book::load(&ctx.alpaca, &ctx.strategy), ctx.alpaca.clock(),)
                .map_err(failed)?;
        let equity = book.equity();
        let positions: Vec<Value> = book
            .positions
            .iter()
            .map(|h| {
                json!({
                    "symbol": h.symbol,
                    "qty": h.qty,
                    "avg_cost": round2(h.avg_cost),
                    "current_price": h.price,
                    "market_value": round2(h.market_value()),
                    "unrealized_pl": round2(h.unrealized_pl()),
                    "unrealized_pl_percent": round2(h.unrealized_pl() / (h.qty * h.avg_cost) * 100.0),
                    "trading_days_held": trading_days_since(h.opened_at),
                    "percent_of_equity": round2(h.market_value() / equity * 100.0),
                })
            })
            .collect();
        let open_orders: Vec<Value> = book
            .open_orders
            .iter()
            .map(|o| {
                json!({
                    "order_id": o.id,
                    "symbol": o.symbol,
                    "side": o.side,
                    "qty": o.qty,
                    "type": o.order_type,
                    "limit_price": o.limit_price,
                    "status": o.status,
                    "created_at": o.created_at,
                })
            })
            .collect();
        Ok(json!({
            "market_open": clock.is_open,
            "next_open": clock.next_open,
            "next_close": clock.next_close,
            "book": {
                "budget": round2(book.budget),
                "equity": round2(equity),
                "return_percent": round2((equity / book.budget - 1.0) * 100.0),
                "cash": round2(book.cash),
                "cash_available": round2(book.available_cash()),
                "reserved_for_open_buys": round2(book.reserved),
                "realized_pl": round2(book.realized_pl),
            },
            "positions": positions,
            "open_orders": open_orders,
            "cash_sweep": book.sweep.as_ref().map(|s| json!({
                "fund": s.symbol,
                "value": round2(s.market_value()),
                "note": "Idle cash parked in T-bills; already included in cash_available. \
                         Warren sells it automatically to fund buys.",
            })),
            "note": "This is your own book within a paper account shared with other agents; \
                     you can only sell shares you bought.",
        }))
    }
}

tool!(
    GetRiskLimits,
    "get_risk_limits",
    NoArgs,
    "The risk limits place_order enforces, and how many orders remain today."
);

impl GetRiskLimits {
    async fn run(&self, _: NoArgs) -> ToolResult {
        let limits = &self.0.config.executor;
        let used = orders_today(&self.0).map_err(failed)?;
        Ok(json!({
            "budget": self.0.strategy.budget,
            "max_positions": limits.max_positions,
            "max_order_notional": round2(limits.max_order_notional),
            "max_position_percent_of_equity": limits.max_position_percent,
            "min_cash_percent_of_equity": limits.min_cash_percent,
            "max_orders_per_day": limits.max_orders_per_day,
            "orders_placed_today": used,
            "allow_short": limits.allow_short,
            "note": "Buys need a research report and a price at or below its entry range; \
                     every other decision is yours.",
        }))
    }
}

/// Why this agent may not buy `symbol`, if it may not: an excluded ticker,
/// or a sector or foreign issuer its universe leaves out.
async fn check_universe(ctx: &AgentCtx, symbol: &str) -> Result<(), String> {
    let universe = &ctx.strategy.universe;
    if universe.excludes(symbol) {
        return Err(format!("{symbol} is on this agent's excluded list"));
    }
    if universe.sectors.is_empty() && universe.allow_foreign {
        return Ok(());
    }
    let Some(sectors) = &ctx.sectors else {
        return Err(
            "this agent limits sectors or foreign issuers, but sector data is off \
                    (set edgar.contact in config.toml)"
                .into(),
        );
    };
    match sectors.profile(symbol).await {
        Ok(Some(profile)) => match profile.exclusion(universe) {
            Some(reason) => Err(format!(
                "{symbol} is outside this agent's universe: {reason}"
            )),
            None => Ok(()),
        },
        Ok(None) | Err(_) => Err(format!(
            "couldn't look up {symbol}'s sector to check this agent's universe"
        )),
    }
}

/// Quotes as of a replayed moment, from daily bars: the last session at or
/// before it. There is no bid or ask in the past.
async fn quotes_as_of(
    ctx: &AgentCtx,
    symbols: &[String],
    as_of: chrono::DateTime<Utc>,
) -> ToolResult {
    let mut quotes = Vec::new();
    for symbol in symbols {
        let bars = ctx
            .alpaca
            .daily_history_until(symbol, as_of - Duration::days(10), Some(as_of))
            .await
            .unwrap_or_default();
        quotes.push(match bars.as_slice() {
            [.., prev, last] => json!({
                "symbol": symbol,
                "price": last.close,
                "change_percent": round2((last.close / prev.close - 1.0) * 100.0),
                "previous_close": prev.close,
                "day_high": last.high,
                "day_low": last.low,
                "day_volume": last.volume,
                "as_of": last.timestamp,
            }),
            _ => json!({ "symbol": symbol, "error": "no data at this date" }),
        });
    }
    Ok(json!({
        "quotes": quotes,
        "note": format!("Replaying {}: prices are daily closes.", as_of.format("%Y-%m-%d")),
    }))
}

/// Weekdays from `since` to today, a close enough count of trading days.
fn trading_days_since(since: chrono::DateTime<Utc>) -> usize {
    let (mut day, today) = (since.date_naive(), Utc::now().date_naive());
    let mut count = 0;
    while day < today {
        day = day.succ_opt().unwrap_or(today);
        if chrono::Datelike::weekday(&day).number_from_monday() <= 5 {
            count += 1;
        }
    }
    count
}

fn orders_today(ctx: &AgentCtx) -> anyhow::Result<usize> {
    let today = Local::now().date_naive();
    Ok(ctx
        .store
        .journal()?
        .iter()
        .filter(|e| e.decided_at.with_timezone(&Local).date_naive() == today)
        .count())
}

// ---------------------------------------------------------------------------
// Trading
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct PlaceOrderArgs {
    pub symbol: String,
    pub side: OrderSideArg,
    /// Whole shares, at least 1.
    pub qty: u32,
    /// Omit for a market order; set for a day limit order.
    pub limit_price: Option<f64>,
    /// Why you are placing this order: the thesis, sizing, and exit plan. This
    /// is recorded in the trade journal and reviewed later.
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
#[serde(rename_all = "lowercase")]
pub enum OrderSideArg {
    Buy,
    Sell,
}

impl From<OrderSideArg> for OrderSide {
    fn from(side: OrderSideArg) -> Self {
        match side {
            OrderSideArg::Buy => OrderSide::Buy,
            OrderSideArg::Sell => OrderSide::Sell,
        }
    }
}

tool!(
    PlaceOrder,
    "place_order",
    PlaceOrderArgs,
    "Place a paper order (day market or limit, whole shares). Checked against the risk \
     limits first; a refusal explains which limit it would break. Every order is journaled \
     with your rationale."
);

impl PlaceOrder {
    async fn run(&self, args: PlaceOrderArgs) -> ToolResult {
        let ctx = &self.0;
        let symbol = args.symbol.trim().to_uppercase();
        let side = OrderSide::from(args.side);
        if args.rationale.trim().len() < 20 {
            return Err(ToolExecutionError::invalid_args(
                "give a real rationale: the thesis, sizing, and exit plan",
            ));
        }

        let alpaca = &ctx.alpaca;
        if ctx.strategy.cash_sweep.as_deref() == Some(symbol.as_str()) {
            return Err(ToolExecutionError::refused(format!(
                "{symbol} is this agent's cash sweep, which Warren manages; it counts as cash"
            )));
        }
        if side == OrderSide::Buy {
            check_universe(ctx, &symbol)
                .await
                .map_err(ToolExecutionError::refused)?;
        }
        let quote_symbols = vec![symbol.clone(), BENCHMARK.to_string()];
        let (account, book, snapshots, asset) = tokio::try_join!(
            alpaca.account(),
            Book::load(alpaca, &ctx.strategy),
            alpaca.snapshots(&quote_symbols),
            alpaca.asset(&symbol),
        )
        .map_err(failed)?;
        let price = snapshots
            .get(&symbol)
            .and_then(|s| s.last_price())
            .ok_or_else(|| ToolExecutionError::not_found(format!("no price for {symbol}")))?;
        let report = ctx.store.report(&symbol).map_err(failed)?;
        // Judged on today's bars, not the report's, which can be a day old.
        let proposal = executor::Proposal {
            side,
            qty: args.qty,
            price: args.limit_price.unwrap_or(price),
            tradable: asset.tradable && asset.status == "active",
            held_qty: book.held(&symbol),
            equity: book.equity(),
            cash: book.available_cash(),
            open_positions: book.positions.len(),
            orders_today: orders_today(ctx).map_err(failed)?,
            limit_price: args.limit_price,
            last_price: price,
            research: report.as_ref().map(|r| executor::ResearchView {
                entry_high: r.findings.entry_high,
            }),
        };
        executor::check_risk(&proposal, &ctx.config.executor)
            .map_err(ToolExecutionError::refused)?;
        // Agents' budgets can add up to more than the account holds.
        let notional = proposal.price * args.qty as f64;
        // Selling this agent's sweep shares frees cash for the buy.
        if side == OrderSide::Buy && notional > account.cash + book.sweep_value() {
            return Err(ToolExecutionError::refused(format!(
                "the shared paper account has only ${:.2} cash left across all agents",
                account.cash
            )));
        }

        // Free up swept cash first, if the buy needs it.
        if side == OrderSide::Buy {
            sweep::fund(ctx, &book, notional).await.map_err(failed)?;
        }
        let new_order = match args.limit_price {
            Some(limit) => NewOrder::limit(&symbol, side, args.qty, limit),
            None => NewOrder::market(&symbol, side, args.qty),
        }
        .with_client_order_id(ctx.strategy.new_order_id());
        let order = alpaca.submit_order(&new_order).await.map_err(failed)?;

        let entry = JournalEntry {
            decided_at: Utc::now(),
            symbol: symbol.clone(),
            side,
            qty: args.qty,
            order_type: order.order_type,
            limit_price: args.limit_price,
            order_id: order.id.clone(),
            price_at_decision: price,
            benchmark_price_at_decision: snapshots.get(BENCHMARK).and_then(|s| s.last_price()),
            rationale: args.rationale,
            research_generated_at: report.as_ref().map(|r| r.generated_at),
            research_factors: report.as_ref().map(|r| r.factors.clone()),
            research_shadow: report.as_ref().and_then(|r| r.shadow.clone()),
        };
        ctx.store.append_journal(&entry).map_err(failed)?;

        Ok(json!({
            "order_id": order.id,
            "status": order.status,
            "symbol": symbol,
            "side": side,
            "qty": args.qty,
            "type": order.order_type,
            "reference_price": price,
            "estimated_notional": round2(price * args.qty as f64),
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct CancelOrderArgs {
    pub order_id: String,
    /// Why the order is no longer wanted.
    pub reason: String,
}

tool!(
    CancelOrder,
    "cancel_order",
    CancelOrderArgs,
    "Cancel an open order by id."
);

impl CancelOrder {
    async fn run(&self, args: CancelOrderArgs) -> ToolResult {
        let ctx = &self.0;
        let order = ctx.alpaca.order(&args.order_id).await.map_err(failed)?;
        if !order
            .client_order_id
            .starts_with(&ctx.strategy.order_prefix())
        {
            return Err(ToolExecutionError::refused(
                "that order belongs to another agent",
            ));
        }
        ctx.alpaca
            .cancel_order(&args.order_id)
            .await
            .map_err(failed)?;
        Ok(json!({ "canceled": args.order_id, "reason": args.reason }))
    }
}

// ---------------------------------------------------------------------------
// Learning
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct ReviewArgs {
    /// How many of the most recent decisions to review, 1 to 50.
    pub limit: u32,
}

tool!(
    ReviewTradeHistory,
    "review_trade_history",
    ReviewArgs,
    "Your past decisions from the trade journal and how each has played out: fill price, \
     return since the decision, SPY's return over the same span, and the excess return. \
     For sells, return is the move you avoided (positive means the price fell after you sold). \
     Includes the rationale and research factor scores you acted on, plus aggregate hit \
     rate."
);

impl ReviewTradeHistory {
    async fn run(&self, args: ReviewArgs) -> ToolResult {
        let ctx = &self.0;
        let journal = ctx.store.journal().map_err(failed)?;
        let recent: Vec<&JournalEntry> = journal
            .iter()
            .rev()
            .take(args.limit.clamp(1, 50) as usize)
            .collect();
        if recent.is_empty() {
            return Ok(json!({ "decisions": [], "note": "No trades yet." }));
        }

        let mut symbols: Vec<String> = recent.iter().map(|e| e.symbol.clone()).collect();
        symbols.push(BENCHMARK.into());
        let symbols = normalize(&symbols);
        let snapshots = ctx.alpaca.snapshots(&symbols).await.map_err(failed)?;
        let price_now = |symbol: &str| snapshots.get(symbol).and_then(|s| s.last_price());

        let fills: HashMap<String, (String, Option<f64>)> =
            futures::future::join_all(recent.iter().map(|e| ctx.alpaca.order(&e.order_id)))
                .await
                .into_iter()
                .flatten()
                .map(|o| (o.id.clone(), (o.status.clone(), o.filled_avg_price)))
                .collect();

        let mut excess_returns = Vec::new();
        let decisions: Vec<Value> = recent
            .iter()
            .map(|e| {
                let (status, fill) = fills
                    .get(&e.order_id)
                    .cloned()
                    .unwrap_or_else(|| ("unknown".into(), None));
                let reference = fill.unwrap_or(e.price_at_decision);
                let direction = match e.side {
                    OrderSide::Buy => 1.0,
                    OrderSide::Sell => -1.0,
                };
                let ret = price_now(&e.symbol)
                    .map(|now| direction * (now - reference) / reference * 100.0);
                let bench = e
                    .benchmark_price_at_decision
                    .zip(price_now(BENCHMARK))
                    .map(|(then, now)| direction * (now - then) / then * 100.0);
                let excess = ret.zip(bench).map(|(r, b)| r - b);
                if fill.is_some() {
                    excess_returns.extend(excess);
                }
                json!({
                    "decided_at": e.decided_at,
                    "days_ago": (Utc::now() - e.decided_at).num_days(),
                    "symbol": e.symbol,
                    "side": e.side,
                    "qty": e.qty,
                    "order_status": status,
                    "fill_price": fill,
                    "price_now": price_now(&e.symbol),
                    "return_percent": ret.map(round2),
                    "spy_return_percent": bench.map(round2),
                    "excess_return_percent": excess.map(round2),
                    "research_factors": e.research_factors.as_ref().map(|f| {
                        f.named()
                            .iter()
                            .map(|(name, factor)| (name.to_string(), json!(factor.score)))
                            .collect::<serde_json::Map<_, _>>()
                    }),
                    "rationale": e.rationale,
                })
            })
            .collect();

        let filled = excess_returns.len();
        let wins = excess_returns.iter().filter(|r| **r > 0.0).count();
        Ok(json!({
            "summary": {
                "decisions_reviewed": decisions.len(),
                "filled": filled,
                "beat_spy": wins,
                "hit_rate_percent": (filled > 0).then(|| round2(wins as f64 / filled as f64 * 100.0)),
                "avg_excess_return_percent": (filled > 0)
                    .then(|| round2(excess_returns.iter().sum::<f64>() / filled as f64)),
            },
            "decisions": decisions,
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct LessonArgs {
    /// A specific, actionable rule learned from reviewing past decisions,
    /// e.g. "Avoid adding to positions down more than 8% within a week of
    /// entry; 3 of 4 such adds underperformed SPY."
    pub lesson: String,
}

tool!(
    RecordLesson,
    "record_lesson",
    LessonArgs,
    "Save a lesson learned from reviewing past decisions. Lessons are shown to you at the \
     start of every future run. Record only well-supported, actionable lessons."
);

impl RecordLesson {
    async fn run(&self, args: LessonArgs) -> ToolResult {
        let lesson = args.lesson.trim().to_string();
        if lesson.len() < 20 {
            return Err(ToolExecutionError::invalid_args(
                "a lesson should be a specific, actionable sentence",
            ));
        }
        self.0.store.add_lesson(lesson).map_err(failed)?;
        Ok(json!({ "saved": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dedupes_and_uppercases() {
        let symbols = normalize(&[" aapl".into(), "AAPL".into(), "".into(), "msft".into()]);
        assert_eq!(symbols, vec!["AAPL", "MSFT"]);
    }

    #[test]
    fn schemas_are_objects() {
        for schema in [
            schema::<PlaceOrderArgs>(),
            schema::<NoArgs>(),
            schema::<NewsArgs>(),
        ] {
            assert_eq!(schema["type"], "object", "{schema}");
        }
    }
}
