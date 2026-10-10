//! The executor agent: turns research into paper trades within the configured
//! risk limits, journals each decision, and learns from how past decisions
//! played out.

use anyhow::{Result, anyhow};
use chrono::Utc;

use super::{AgentCtx, AgentKind, tools::*};
use crate::config::ExecutorConfig;
use crate::market::alpaca::OrderSide;

/// An order as the risk check sees it.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub side: OrderSide,
    pub qty: u32,
    /// Limit price, or the last trade for market orders.
    pub price: f64,
    pub tradable: bool,
    /// Shares currently held; negative when short.
    pub held_qty: f64,
    /// The strategy's book: equity, and cash not committed to open buys.
    pub equity: f64,
    pub cash: f64,
    /// Positions the strategy holds now.
    pub open_positions: usize,
    pub orders_today: usize,
    /// Set for limit orders.
    pub limit_price: Option<f64>,
    /// Latest trade.
    pub last_price: f64,
    /// The latest research report's view, if any.
    pub research: Option<ResearchView>,
}

#[derive(Debug, Clone, Copy)]
pub struct ResearchView {
    /// Top of the report's entry range: buys may not be priced above it.
    pub entry_high: Option<f64>,
}

/// Enforce the risk limits. The error explains which limit the order would
/// break, for the model to read.
pub fn check_risk(p: &Proposal, limits: &ExecutorConfig) -> Result<(), String> {
    if !p.tradable {
        return Err("this symbol is not an active, tradable asset".into());
    }
    if p.qty == 0 || p.price <= 0.0 {
        return Err("quantity and price must be positive".into());
    }
    if p.orders_today >= limits.max_orders_per_day {
        return Err(format!(
            "daily order limit reached ({} orders)",
            limits.max_orders_per_day
        ));
    }
    let notional = p.qty as f64 * p.price;
    if notional > limits.max_order_notional {
        return Err(format!(
            "order notional ${notional:.2} exceeds the ${:.2} per-order limit; at most {} shares",
            limits.max_order_notional,
            (limits.max_order_notional / p.price).floor()
        ));
    }

    match p.side {
        OrderSide::Buy => {
            let covering_short = p.held_qty < 0.0;
            if !covering_short {
                let Some(research) = p.research else {
                    return Err(
                        "no research report for this symbol; buys need one, since its entry \
                         range caps the price"
                            .into(),
                    );
                };
                check_entry(p, research.entry_high)?;
            }
            if p.held_qty <= 0.0 && p.open_positions >= limits.max_positions {
                return Err(format!(
                    "already holding {} positions, the most allowed; sell one first",
                    p.open_positions
                ));
            }
            let position_after = (p.held_qty.max(0.0) + p.qty as f64) * p.price;
            let max_position = limits.max_position_percent / 100.0 * p.equity;
            if position_after > max_position {
                return Err(format!(
                    "position would be ${position_after:.2}, above the {:.1}% of equity cap \
                     (${max_position:.2})",
                    limits.max_position_percent
                ));
            }
            let min_cash = limits.min_cash_percent / 100.0 * p.equity;
            if p.cash - notional < min_cash {
                return Err(format!(
                    "cash would fall to ${:.2}, below the {:.1}% reserve (${min_cash:.2})",
                    p.cash - notional,
                    limits.min_cash_percent
                ));
            }
        }
        OrderSide::Sell => {
            if !limits.allow_short && p.qty as f64 > p.held_qty {
                return Err(format!(
                    "short selling is disabled and you hold {} shares",
                    p.held_qty.max(0.0)
                ));
            }
        }
    }
    Ok(())
}

/// Buys may not chase: the order must be priced at or below the top of the
/// research entry range.
fn check_entry(p: &Proposal, entry_high: Option<f64>) -> Result<(), String> {
    let Some(entry_high) = entry_high else {
        return Err("the research report gives no entry range; buys need one".into());
    };
    match p.limit_price {
        Some(limit) if limit > entry_high => Err(format!(
            "limit {limit:.2} is above the research entry range (top {entry_high:.2}); \
             place the limit at or below {entry_high:.2} and let the price come to you"
        )),
        None if p.last_price > entry_high => Err(format!(
            "price {:.2} is above the research entry range (top {entry_high:.2}); use a \
             limit order at or below {entry_high:.2} instead of chasing",
            p.last_price
        )),
        _ => Ok(()),
    }
}

fn preamble(ctx: &AgentCtx) -> Result<String> {
    let limits = &ctx.config.executor;
    let lessons = ctx.store.lessons()?;
    let lessons = if lessons.is_empty() {
        "None yet.".to_string()
    } else {
        lessons
            .iter()
            .map(|l| format!("- ({}) {}", l.created_at.format("%Y-%m-%d"), l.text))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(format!(
        "\
You are the executor for one of Warren's autonomous trading agents (US \
equities; its holding period and universe are below). It trades its own \
budget within an Alpaca paper account shared with other agents. You make \
every trading decision: what to buy, hold, trim, or sell, how much, and \
when. Research reports, prices, technicals, and news are your evidence; no \
score decides for you. Nobody reviews your orders before they go out, so be \
deliberate.

Each run:
1. review_trade_history (about 20 decisions) to see how your past calls \
   played out against SPY. If a clear, repeated pattern shows up, \
   record_lesson with a specific, actionable rule (at most two per run, and \
   do not repeat existing lessons).
2. get_portfolio and get_risk_limits.
3. list_research, then get_research_report for anything you might act on. \
   Read the thesis, the risks, the news highlights, and the levels.
4. Check what you need yourself: get_quotes, get_technicals, \
   get_price_history, get_news. A report can be up to a day or two old.
5. Manage existing positions first: decide whether each still deserves its \
   place given its thesis, risks, stop, and how it has traded.
6. Then decide on new buys. Weigh the evidence and the risks; it is fine \
   to buy nothing.
7. Cancel stale open orders that no longer fit the plan.
8. Finish with a short summary of what you did and why, including decisions \
   not to trade.

Judgment aids (yours to use, not rules):
- Sizing: risking roughly 0.5–1% of equity per trade, from the distance to \
  the research stop, keeps one bad call from hurting much; size up only for \
  your strongest views. Keep the book diversified; avoid piling into one \
  theme.
- Factor scores (1 = very bearish, 5 = very bullish) are context. What the \
  evidence says so far: in backtests over 2017–2026 and in replays of the \
  research agent's own judgments, not buying stretched prices (extension) \
  held up most consistently; trend, momentum, and relative strength showed \
  no edge at horizons of a few weeks, and the strongest recent movers tended \
  to reverse. The judged catalyst, news, and setup scores have not yet shown \
  an edge either.
- Trend health in reports (distance below the 52-week high, 12-1 month \
  return): stocks far below their highs or down over the year kept lagging.
- Trading often cost about 3% a year in backtests; don't swap a holding for \
  a marginally better idea.

Hard limits (place_order enforces these and refuses violations):
- Max ${max_notional:.0} per order; max {max_pos:.1}% of equity per position; at \
  most {max_positions} positions.
- Keep at least {min_cash:.1}% of equity in cash.
- At most {max_orders} orders per day. Shorting {short}.
- Only stocks within this agent's universe.
- Buys need a research report, and must be priced at or below the top of \
  its entry range: a limit at or below it, or a market order only when the \
  price is already inside or below it. Do not chase; if the price is above \
  the range, place a resting limit order or wait.

Lessons from your past decisions:
{lessons}",
        max_notional = limits.max_order_notional,
        max_pos = limits.max_position_percent,
        max_positions = limits.max_positions,
        min_cash = limits.min_cash_percent,
        max_orders = limits.max_orders_per_day,
        short = if limits.allow_short {
            "is allowed"
        } else {
            "is disabled"
        },
    ))
}

/// Run one executor cycle and return its summary.
pub async fn run(ctx: &AgentCtx) -> Result<String> {
    let config = &ctx.config.executor.agent;
    ctx.recorded(AgentKind::Executor, None, async |ctx| {
        let agent = ctx
            .builder(AgentKind::Executor, None, config, preamble(ctx)?)?
            .tool(ReviewTradeHistory(ctx.clone()))
            .tool(RecordLesson(ctx.clone()))
            .tool(GetPortfolio(ctx.clone()))
            .tool(GetRiskLimits(ctx.clone()))
            .tool(ListResearch(ctx.clone()))
            .tool(GetResearchReport(ctx.clone()))
            .tool(GetQuotes(ctx.clone()))
            .tool(GetTechnicals(ctx.clone()))
            .tool(GetPriceHistory(ctx.clone()))
            .tool(GetNews(ctx.clone()))
            .tool(PlaceOrder(ctx.clone()))
            .tool(CancelOrder(ctx.clone()))
            .build();

        let response = agent
            .prompt(format!(
                "Run your trading cycle now ({}).",
                Utc::now().format("%Y-%m-%d %H:%M UTC")
            ))
            .max_turns(config.max_turns)
            .await
            .map_err(|err| anyhow!("executor run failed: {err}"))?;
        let summary = response.output();
        Ok((summary.clone(), summary))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy(qty: u32, price: f64) -> Proposal {
        Proposal {
            side: OrderSide::Buy,
            qty,
            price,
            tradable: true,
            held_qty: 0.0,
            equity: 100_000.0,
            cash: 100_000.0,
            open_positions: 0,
            orders_today: 0,
            limit_price: Some(price),
            last_price: price,
            research: Some(ResearchView {
                entry_high: Some(price),
            }),
        }
    }

    #[test]
    fn caps_the_number_of_positions() {
        let limits = ExecutorConfig::default();
        let mut p = buy(10, 100.0);
        p.open_positions = limits.max_positions;
        assert!(check_risk(&p, &limits).unwrap_err().contains("positions"));
        // Adding to a position already held is fine.
        p.held_qty = 5.0;
        assert!(check_risk(&p, &limits).is_ok());
    }

    #[test]
    fn refuses_to_chase_above_the_entry_range() {
        let limits = ExecutorConfig::default();
        let mut p = buy(10, 100.0);
        p.research.as_mut().unwrap().entry_high = Some(98.0);

        // Limit above the range.
        let err = check_risk(&p, &limits).unwrap_err();
        assert!(err.contains("above the research entry range"), "{err}");

        // Limit at the top of the range is fine, even with the price above it.
        p.limit_price = Some(98.0);
        p.price = 98.0;
        assert_eq!(check_risk(&p, &limits), Ok(()));

        // Market order while the price is above the range.
        p.limit_price = None;
        p.price = 100.0;
        assert!(check_risk(&p, &limits).is_err());

        // Market order once the price is back in the range.
        p.last_price = 97.0;
        p.price = 97.0;
        assert_eq!(check_risk(&p, &limits), Ok(()));
    }

    #[test]
    fn buys_need_an_entry_range() {
        let mut p = buy(10, 100.0);
        p.research.as_mut().unwrap().entry_high = None;
        let err = check_risk(&p, &ExecutorConfig::default()).unwrap_err();
        assert!(err.contains("no entry range"), "{err}");
    }

    #[test]
    fn covering_a_short_skips_research_checks() {
        let mut p = buy(10, 100.0);
        p.held_qty = -10.0;
        p.research = None;
        assert_eq!(check_risk(&p, &ExecutorConfig::default()), Ok(()));
    }

    #[test]
    fn allows_a_sized_buy() {
        assert_eq!(
            check_risk(&buy(20, 300.0), &ExecutorConfig::default()),
            Ok(())
        );
    }

    #[test]
    fn enforces_order_notional() {
        let err = check_risk(&buy(40, 300.0), &ExecutorConfig::default()).unwrap_err();
        assert!(err.contains("at most 33 shares"), "{err}");
    }

    #[test]
    fn enforces_position_cap_including_held_shares() {
        let mut p = buy(20, 300.0);
        p.held_qty = 20.0;
        let err = check_risk(&p, &ExecutorConfig::default()).unwrap_err();
        assert!(err.contains("% of equity cap"), "{err}");
    }

    #[test]
    fn buys_need_research_but_no_verdict() {
        let mut p = buy(10, 100.0);
        p.research = None;
        let err = check_risk(&p, &ExecutorConfig::default()).unwrap_err();
        assert!(err.contains("no research report"), "{err}");
        // Any report with an entry range will do: the decision is the
        // executor's.
        p.research = Some(ResearchView {
            entry_high: Some(100.0),
        });
        assert_eq!(check_risk(&p, &ExecutorConfig::default()), Ok(()));
    }

    #[test]
    fn sells_are_never_held_back() {
        let mut p = buy(5, 100.0);
        p.side = OrderSide::Sell;
        p.held_qty = 10.0;
        p.research = None;
        assert_eq!(check_risk(&p, &ExecutorConfig::default()), Ok(()));
    }

    #[test]
    fn keeps_cash_reserve() {
        let mut p = buy(30, 300.0);
        p.cash = 15_000.0;
        let err = check_risk(&p, &ExecutorConfig::default()).unwrap_err();
        assert!(err.contains("reserve"), "{err}");
    }

    #[test]
    fn blocks_shorts_unless_allowed() {
        let mut p = buy(10, 100.0);
        p.side = OrderSide::Sell;
        p.held_qty = 5.0;
        assert!(check_risk(&p, &ExecutorConfig::default()).is_err());
        p.held_qty = 10.0;
        assert_eq!(check_risk(&p, &ExecutorConfig::default()), Ok(()));
        let limits = ExecutorConfig {
            allow_short: true,
            ..Default::default()
        };
        p.held_qty = 0.0;
        assert_eq!(check_risk(&p, &limits), Ok(()));
    }

    #[test]
    fn stops_at_daily_order_limit() {
        let mut p = buy(1, 100.0);
        p.orders_today = 10;
        assert!(check_risk(&p, &ExecutorConfig::default()).is_err());
    }
}
