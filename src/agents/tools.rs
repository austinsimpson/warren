//! Tools the agents call. Each wraps the Alpaca client or the store and
//! returns compact JSON. Failures go back to the model as tool errors it can
//! read and react to, rather than aborting the run.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, Local, Utc};
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use rig_core::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentCtx, executor, technicals};
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
        let start = Utc::now() - Duration::days(550);
        let bars = self
            .0
            .alpaca
            .bars(&symbol, Timeframe::Day, start)
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
            .bars(&symbol, timeframe, Utc::now() - Duration::days(days.into()))
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
        let start = Utc::now() - Duration::days(args.days.clamp(1, 30).into());
        let news = self
            .0
            .alpaca
            .news(&symbols, start, args.limit.clamp(1, 50))
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
        let (actives, movers) =
            tokio::try_join!(alpaca.most_actives(top), alpaca.movers(top)).map_err(failed)?;

        // The most-actives screener has no prices; look them up.
        let active_symbols: Vec<String> = actives.iter().map(|a| a.symbol.clone()).collect();
        let prices = alpaca.snapshots(&active_symbols).await.map_err(failed)?;
        let actives: Vec<Value> = actives
            .iter()
            .filter_map(|a| {
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
                .filter(|m| m.price >= min_price)
                .map(|m| json!({ "symbol": m.symbol, "price": m.price, "change_percent": round2(m.percent_change) }))
                .collect()
        };
        Ok(json!({
            "min_price_filter": min_price,
            "most_active": actives,
            "gainers": movers_above(&movers.gainers),
            "losers": movers_above(&movers.losers),
        }))
    }
}

// ---------------------------------------------------------------------------
// Research and portfolio state
// ---------------------------------------------------------------------------

tool!(
    GetResearchReport,
    "get_research_report",
    SymbolArgs,
    "The latest full research report for a symbol, including when it was written."
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
    "One line per researched symbol: rating and conviction (both computed from factor \
     scores; conviction 0 = very bearish, 0.5 = neutral, 1 = very bullish), reward-to-risk, \
     target, stop, and report age."
);

impl ListResearch {
    async fn run(&self, _: NoArgs) -> ToolResult {
        let reports = self.0.store.reports().map_err(failed)?;
        let rows: Vec<Value> = reports
            .iter()
            .map(|r| {
                json!({
                    "symbol": r.symbol,
                    "rating": r.rating,
                    "conviction": r.conviction,
                    "risk_reward": r.risk_reward.map(round2),
                    "price_at_research": r.price_at_research,
                    "target_price": r.findings.target_price,
                    "stop_loss": r.findings.stop_loss,
                    "horizon_days": r.findings.horizon_days,
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
        let alpaca = &self.0.alpaca;
        let (account, positions, orders, clock) = tokio::try_join!(
            alpaca.account(),
            alpaca.positions(),
            alpaca.recent_orders(50),
            alpaca.clock(),
        )
        .map_err(failed)?;
        let positions: Vec<Value> = positions
            .iter()
            .map(|p| {
                json!({
                    "symbol": p.symbol,
                    "qty": p.qty,
                    "avg_entry_price": p.avg_entry_price,
                    "current_price": p.current_price,
                    "market_value": p.market_value,
                    "unrealized_pl": p.unrealized_pl,
                    "unrealized_pl_percent": p.unrealized_plpc.map(|f| round2(f * 100.0)),
                    "percent_of_equity": p.market_value.map(|v| round2(v / account.equity * 100.0)),
                })
            })
            .collect();
        let open_orders: Vec<Value> = orders
            .iter()
            .filter(|o| o.is_open())
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
            "account": {
                "equity": account.equity,
                "cash": account.cash,
                "buying_power": account.buying_power,
                "day_change": round2(account.day_change()),
                "day_change_percent": account.day_change_percent().map(round2),
                "trading_blocked": account.trading_blocked,
            },
            "positions": positions,
            "open_orders": open_orders,
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
            "max_order_notional": limits.max_order_notional,
            "max_position_percent_of_equity": limits.max_position_percent,
            "min_cash_percent_of_equity": limits.min_cash_percent,
            "max_orders_per_day": limits.max_orders_per_day,
            "orders_placed_today": used,
            "allow_short": limits.allow_short,
            "min_conviction_to_buy": limits.min_conviction_to_buy,
            "note": "Buys need a research report rated buy or strong_buy at or above min_conviction_to_buy.",
        }))
    }
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
        let quote_symbols = vec![symbol.clone(), BENCHMARK.to_string()];
        let (account, positions, snapshots, asset) = tokio::try_join!(
            alpaca.account(),
            alpaca.positions(),
            alpaca.snapshots(&quote_symbols),
            alpaca.asset(&symbol),
        )
        .map_err(failed)?;
        let price = snapshots
            .get(&symbol)
            .and_then(|s| s.last_price())
            .ok_or_else(|| ToolExecutionError::not_found(format!("no price for {symbol}")))?;
        let report = ctx.store.report(&symbol).map_err(failed)?;

        let proposal = executor::Proposal {
            side,
            qty: args.qty,
            price: args.limit_price.unwrap_or(price),
            tradable: asset.tradable && asset.status == "active",
            held_qty: positions
                .iter()
                .find(|p| p.symbol == symbol)
                .map_or(0.0, |p| p.qty),
            equity: account.equity,
            cash: account.cash,
            orders_today: orders_today(ctx).map_err(failed)?,
            limit_price: args.limit_price,
            last_price: price,
            research: report.as_ref().map(|r| executor::ResearchView {
                rating: r.rating,
                conviction: r.conviction,
                entry_high: r.findings.entry_high,
            }),
        };
        executor::check_risk(&proposal, &ctx.config.executor)
            .map_err(ToolExecutionError::refused)?;

        let new_order = match args.limit_price {
            Some(limit) => NewOrder::limit(&symbol, side, args.qty, limit),
            None => NewOrder::market(&symbol, side, args.qty),
        };
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
            research_rating: report.as_ref().map(|r| r.rating.to_string()),
            research_conviction: report.as_ref().map(|r| r.conviction),
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
        self.0
            .alpaca
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
     Includes the rationale and research rating you acted on, plus aggregate hit rate."
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
                    "research_rating": e.research_rating,
                    "research_conviction": e.research_conviction,
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
