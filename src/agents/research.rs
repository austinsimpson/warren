//! The research agent: an in-depth analysis of one symbol, returned as a
//! typed [`ResearchReport`]: the thesis, risks, levels, dated news
//! highlights, and factor scores, all context for the executor, which makes
//! every trading decision itself.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rig_core::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::strategy::Style;

use super::{
    AgentCtx, AgentKind,
    book::Book,
    jev::ShadowScores,
    scoring::{self, FactorScores, JudgedFactors, QuantFactors},
    technicals::{self, Technicals},
    tools::*,
};
use std::future::IntoFuture;

/// What the model produces.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct ResearchFindings {
    /// Expected holding period in trading days.
    pub horizon_days: u32,
    /// The core argument in 2–4 sentences.
    pub thesis: String,
    /// Specific upcoming events or drivers that could move the stock.
    pub catalysts: Vec<String>,
    /// The key risks, most serious first: what could go wrong, how likely
    /// it looks, and what would show it happening.
    pub risks: Vec<String>,
    /// The stock's most important recent news, newest first.
    #[serde(default)]
    pub news_highlights: Vec<NewsHighlight>,
    /// Lower bound of a sensible entry range, if buying.
    pub entry_low: Option<f64>,
    /// Upper bound of a sensible entry range, if buying.
    pub entry_high: Option<f64>,
    /// Price target over the horizon.
    pub target_price: Option<f64>,
    /// Price at which the thesis is invalidated.
    pub stop_loss: Option<f64>,
    /// Trend, momentum, and levels, citing the indicator values.
    pub technical_summary: String,
    /// What recent news means for the stock.
    pub news_summary: String,
    /// Your scores for the judgment factors. Trend, momentum, and relative
    /// strength are computed by Warren; see `scoring`.
    pub factors: JudgedFactors,
}

/// One dated news item and why it matters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct NewsHighlight {
    /// YYYY-MM-DD.
    pub date: String,
    pub headline: String,
    /// One sentence: what it means for the stock and whether the price
    /// already reflects it.
    pub significance: String,
}

/// Bump when the report format or factor definitions change.
///
/// - 1: conviction from every factor.
/// - 2: trend, momentum, and relative strength out of conviction.
/// - 3: no conviction or rating; factors are context, and the executor
///   decides.
pub const SCORING_VERSION: u32 = 3;

/// How broken a stock's long-run trend is, as context. Among stocks like the
/// ones exploration screens, those far below their 52-week high or down
/// over the past year (skipping the last month) kept lagging; see
/// `backtest`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrendHealth {
    pub percent_below_52w_high: f64,
    /// `None` with under about a year of history.
    pub return_12_1_percent: Option<f64>,
}

impl TrendHealth {
    pub fn from_technicals(t: &Technicals) -> Self {
        Self {
            percent_below_52w_high: t.percent_below_52w_high,
            return_12_1_percent: t.return_12_1_percent,
        }
    }
}

/// A stored report: the findings, the scores computed from them, and when and
/// at what price they were made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchReport {
    pub symbol: String,
    pub generated_at: DateTime<Utc>,
    pub model: String,
    pub price_at_research: Option<f64>,
    /// The [`SCORING_VERSION`] the report was written under.
    #[serde(default)]
    pub scoring_version: u32,
    /// Computed reward-to-risk of the entry, target, and stop, if a long setup.
    pub risk_reward: Option<f64>,
    /// Every factor, computed and judged.
    pub factors: FactorScores,
    /// Long-run price health; absent on reports written before it was
    /// recorded.
    #[serde(default)]
    pub trend_health: Option<TrendHealth>,
    /// Fundamentals from SEC filings as of the research moment; absent for
    /// companies without US-GAAP filings and on older reports.
    #[serde(default)]
    pub fundamentals: Option<crate::fundamentals::Snapshot>,
    /// Jev's scores for the same judgment factors, recorded for comparison.
    #[serde(default)]
    pub shadow: Option<ShadowScores>,
    pub findings: ResearchFindings,
}

const PREAMBLE: &str = "\
You are the research analyst for one of Warren's autonomous trading agents \
(US equities, long-biased; its holding period and universe are below). You write \
one in-depth report on one stock. An executor agent decides whether and how to \
trade from your report, so be specific, calibrated, and candid about risk. You \
do not make the decision and give no rating.

Process:
1. get_asset to confirm it is an active, tradable listed equity.
2. get_quotes and get_technicals for price, levels, and volatility.
3. get_price_history (daily, ~6 months) to judge structure: support and \
   resistance, recent breakouts or breakdowns, gaps.
4. get_news (about 14 days) for catalysts, earnings, guidance, and sentiment.
5. list_filings and read_filing for the company's own words: the latest \
   earnings release (an 8-K flagged earnings), and the 10-Q or 10-K \
   (Results of Operations, Liquidity, Outlook) when results matter to the \
   thesis. Cite what you read.

Report:
- Thesis: the core argument in 2–4 sentences, for or against owning it now.
- Risks: the key risks, most serious first. For each, what could go wrong, \
  how likely it looks, and what would show it happening. Always include \
  some, even for a strong setup.
- News highlights: the 3–6 most important recent items, newest first, each \
  dated, with what it means and whether the price already reflects it.
- Levels: see the style section below. The executor may not buy above \
  your entry range, so set it where buying makes sense rather than chasing. \
  Leave levels empty if you see no long setup.
- Factor scores: Warren has computed trend, momentum, relative strength, \
  and extension (how stretched the price is), shown in the request; do not \
  re-score them. Score catalyst, news sentiment, and setup quality (chart \
  structure, not stretch) each from 1 to 5 on its own evidence: 1 = clearly \
  bearish, 2 = leaning bearish, 3 = neutral or no signal, 4 = leaning \
  bullish, 5 = clearly bullish, each with a one-sentence reason citing the \
  data. These are context for the executor, not a verdict.

Rules:
- Ground every claim in tool data; cite indicator values, prices, and dates.
- You have no fundamentals feed beyond what you find; do not invent \
  financial figures.";

/// The part of the prompt that depends on the agent's style.
fn style_section(style: Style) -> &'static str {
    match style {
        Style::Swing => {
            "\n\nSwing style: setups that play out over days to weeks. Give an \
             entry range, a target reachable within the horizon given recent volatility, \
             and a stop beyond a meaningful level, roughly 1–2.5 ATR below entry. Setup \
             quality is about chart structure."
        }
        Style::Growth => {
            "\n\nGrowth style: companies whose revenue is growing fast, at \
             prices that don't already assume perfection, held for months while the \
             thesis holds. Judge how fast and durable growth is and whether it is \
             accelerating; the company's edge and market opportunity; margins, the path \
             to profit, and dilution; and valuation against growth (in backtests, the \
             most expensive stocks lagged even among fast growers). Use the fundamentals \
             in the request and web search for earnings calls, guidance, and filings. \
             Levels: an entry range where buying makes sense; a stop at the price that \
             would break the thesis (a major support or roughly 20–30% below entry, not \
             a tight ATR stop); a target only if you can justify a 6–12 month value; \
             horizon_days of about 60–250. Score setup quality as the business setup \
             (durability of growth and competitive position), not chart structure."
        }
    }
}

/// Research `symbol`, store the report, and return it.
pub async fn run(ctx: &AgentCtx, symbol: &str) -> Result<ResearchReport> {
    let symbol = symbol.trim().to_uppercase();
    let config = &ctx.config.research.agent;
    ctx.recorded(AgentKind::Research, Some(&symbol), async |ctx| {
        let (quant, trend_health) = compute_quant_factors(ctx, &symbol).await?;
        let price = match ctx.as_of {
            // The last close at or before the replayed moment.
            Some(as_of) => ctx
                .alpaca
                .daily_history_until(&symbol, as_of - chrono::Duration::days(10), Some(as_of))
                .await
                .ok()
                .and_then(|bars| bars.last().map(|b| b.close)),
            None => ctx
                .alpaca
                .snapshots(std::slice::from_ref(&symbol))
                .await
                .ok()
                .and_then(|s| s.get(&symbol).and_then(|s| s.last_price())),
        };
        // Only figures filed by the research moment, so replays stay honest.
        let fundamentals = match &ctx.fundamentals {
            Some(directory) => directory
                .series(&symbol)
                .await
                .ok()
                .flatten()
                .map(|series| series.metrics(ctx.now().date_naive()))
                .filter(|m| m.latest_quarter.is_some())
                .map(|m| crate::fundamentals::Snapshot::new(m, price)),
            None => None,
        };
        let fundamentals_note = match &fundamentals {
            Some(f) => format!("\n\nFundamentals from SEC filings: {}.", f.describe()),
            None => "\n\nNo US-GAAP fundamentals on file (foreign issuers usually report \
                     under IFRS); find figures with web search if you need them."
                .to_string(),
        };
        let sector = match &ctx.sectors {
            Some(directory) => match directory.profile(&symbol).await {
                Ok(Some(p)) => format!(
                    "\n\nCompany: {} ({}){}.",
                    p.sector.map_or("sector unknown", |s| s.label()),
                    if p.sic_description.is_empty() {
                        "no SEC industry code"
                    } else {
                        &p.sic_description
                    },
                    if p.foreign {
                        ", incorporated outside the US"
                    } else {
                        ""
                    },
                ),
                _ => String::new(),
            },
            None => String::new(),
        };
        // Anthropic's hosted web search, live only: in a replay its results
        // could be from after the replayed moment.
        let searches = match ctx.as_of {
            None => ctx.config.research.web_searches_per_stock,
            Some(_) => 0,
        };
        // A stock the agent owns gets re-underwritten against its filings.
        let holding_note = holding_note(ctx, &symbol, price).await;
        let preamble = format!("{PREAMBLE}{}", style_section(ctx.strategy.style));
        let mut builder = ctx.builder(AgentKind::Research, Some(&symbol), config, preamble)?;
        if searches > 0 {
            builder = builder.additional_params(serde_json::json!({
                "tools": [{
                    "type": "web_search_20250305",
                    "name": "web_search",
                    "max_uses": searches,
                }],
            }));
        }
        let search_note = if searches > 0 {
            format!(
                "\n\nYou may use web_search up to {searches} times to go beyond the news feed: \
                 earnings releases and guidance, SEC filings, analyst actions, and what the \
                 market already expects. Prefer primary sources, check each result's date, \
                 and judge whether the news is already reflected in the price."
            )
        } else {
            String::new()
        };
        let agent = builder
            .tool(GetAsset(ctx.clone()))
            .tool(GetQuotes(ctx.clone()))
            .tool(GetTechnicals(ctx.clone()))
            .tool(GetPriceHistory(ctx.clone()))
            .tool(GetNews(ctx.clone()))
            .tool(ListFilings(ctx.clone()))
            .tool(ReadFiling(ctx.clone()))
            .build();

        let research = agent
            .prompt_typed::<ResearchFindings>(format!(
                "Research {symbol} now ({}) and return your report.\n\n\
                 Computed factors (fixed; do not re-score):\n{}{sector}{fundamentals_note}\
                 {holding_note}{search_note}",
                ctx.now().format("%Y-%m-%d %H:%M UTC"),
                describe(&quant),
            ))
            .max_turns(config.max_turns)
            .retries(1)
            .into_future();
        let (response, shadow) = tokio::join!(research, shadow_score(ctx, &symbol));
        let response = response.map_err(|err| anyhow!("research on {symbol} failed: {err}"))?;

        let findings = response.output;
        let (risk_reward, factors) = scoring::score(&findings, quant, price);
        let report = ResearchReport {
            symbol: symbol.clone(),
            generated_at: ctx.now(),
            model: config.model.clone(),
            price_at_research: price,
            scoring_version: SCORING_VERSION,
            risk_reward,
            factors,
            shadow,
            fundamentals,
            trend_health: Some(trend_health),
            findings,
        };
        ctx.store.save_report(&report)?;
        let factors: Vec<String> = report
            .factors
            .named()
            .iter()
            .map(|(_, f)| f.score.to_string())
            .collect();
        let shadow = report.shadow.as_ref().map_or(String::new(), |s| {
            format!(
                " jev catalyst {:.1} news {:.1}",
                s.catalyst.score, s.news_sentiment.score
            )
        });
        let searched = match ctx.meter.web_searches() {
            0 => String::new(),
            1 => " · 1 web search".to_string(),
            n => format!(" · {n} web searches"),
        };
        let levels = match (report.findings.entry_high, report.findings.stop_loss) {
            (Some(entry), Some(stop)) => format!(" entry ≤{entry:.2} stop {stop:.2}"),
            _ => " no long setup".to_string(),
        };
        let summary = format!(
            "{symbol} [{}]{levels}, {} risks{shadow}{searched}",
            factors.join(" "),
            report.findings.risks.len(),
        );
        Ok((report, summary))
    })
    .await
}

/// For a stock the agent holds: the position, the previous report's thesis,
/// and a directive to check the latest results against it. Empty otherwise,
/// and in replays, where the book is today's.
async fn holding_note(ctx: &AgentCtx, symbol: &str, price: Option<f64>) -> String {
    if ctx.as_of.is_some() {
        return String::new();
    }
    let Ok(book) = Book::load(&ctx.alpaca, &ctx.strategy).await else {
        return String::new();
    };
    let Some(held) = book.positions.iter().find(|h| h.symbol == symbol) else {
        return String::new();
    };
    let gain = price.map_or(String::new(), |p| {
        format!(", {:+.1}% at {p:.2}", (p / held.avg_cost - 1.0) * 100.0)
    });
    let previous = match ctx.store.report(symbol) {
        Ok(Some(r)) => format!(
            "\nThe previous report ({}) said: {} (entry {}, stop {}.)",
            r.generated_at.format("%Y-%m-%d"),
            r.findings.thesis,
            r.findings
                .entry_high
                .map_or("none".into(), |e| format!("≤{e:.2}")),
            r.findings
                .stop_loss
                .map_or("none".into(), |s| format!("{s:.2}")),
        ),
        _ => String::new(),
    };
    format!(
        "\n\nThis agent holds {symbol}: {:.0} shares at an average cost of {:.2}{gain}, \
         opened {}.{previous}\nBecause it is held, re-underwrite it from the company's \
         own numbers: use list_filings and read_filing to read the latest earnings \
         release and the latest 10-Q or 10-K (results, guidance, margins, cash, and any \
         new risks), compare them with the thesis above, and say plainly in your thesis \
         whether it still holds, has strengthened, or has broken. Set the stop where the \
         thesis would be wrong.",
        held.qty,
        held.avg_cost,
        held.opened_at.format("%Y-%m-%d"),
    )
}

/// Trend, momentum, and relative strength from about 18 months of daily
/// bars for the stock and the benchmark.
/// The computed factors and trend health for `symbol`.
async fn compute_quant_factors(
    ctx: &AgentCtx,
    symbol: &str,
) -> Result<(QuantFactors, TrendHealth)> {
    let start = ctx.now() - chrono::Duration::days(550);
    let (stock, benchmark) = tokio::try_join!(
        ctx.alpaca.daily_history_until(symbol, start, ctx.as_of),
        ctx.alpaca.daily_history_until(BENCHMARK, start, ctx.as_of),
    )?;
    let stock = technicals::compute(&stock).ok_or_else(|| anyhow!("no daily bars for {symbol}"))?;
    let benchmark = technicals::compute(&benchmark);
    Ok((
        scoring::quant_factors(&stock, benchmark.as_ref()),
        TrendHealth::from_technicals(&stock),
    ))
}

/// Jev's catalyst and news scores from the last two weeks of headlines, or
/// `None` (with a status message) if Jev is off or fails. Never fails the run.
async fn shadow_score(ctx: &AgentCtx, symbol: &str) -> Option<ShadowScores> {
    let jev = ctx.jev.as_ref()?;
    let result = async {
        let since = ctx.now() - chrono::Duration::days(14);
        let news = ctx
            .alpaca
            .news(&[symbol.to_string()], since, ctx.as_of, 50)
            .await?;
        jev.score(symbol, &news, ctx.now()).await
    }
    .await;
    match result {
        Ok(scores) => {
            ctx.meter.add_jev(scores.input_tokens, scores.output_tokens);
            Some(scores)
        }
        Err(err) => {
            ctx.status(format!("Jev shadow scoring {symbol} failed: {err:#}"));
            None
        }
    }
}

fn describe(quant: &QuantFactors) -> String {
    [
        ("trend", &quant.trend),
        ("momentum", &quant.momentum),
        ("relative_strength", &quant.relative_strength),
        ("extension", &quant.extension),
    ]
    .iter()
    .map(|(name, factor)| match factor {
        Some(f) => format!("- {name}: {} ({})", f.score, f.reason),
        None => format!("- {name}: not enough history; excluded"),
    })
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_schema_asks_for_context_not_a_verdict() {
        let schema =
            serde_json::to_value(rig_core::schemars::schema_for!(ResearchFindings)).unwrap();
        let properties = &schema["properties"];
        for field in ["factors", "risks", "news_highlights", "thesis"] {
            assert!(properties[field].is_object(), "{field}: {schema}");
        }
        assert!(properties.get("rating").is_none() && properties.get("conviction").is_none());
    }
}

#[cfg(test)]
mod live_tests {
    /// Runs the real research agent (costs API tokens); run with
    /// `cargo test live_research -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_research() {
        let (ctx, dir) = crate::agents::test_support::live_ctx();
        let report = crate::runtime::RUNTIME
            .block_on(super::run(&ctx, "AAPL"))
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert_eq!(ctx.store.report("AAPL").unwrap().unwrap().symbol, "AAPL");
        let run = &ctx.store.recent_runs(1).unwrap()[0];
        println!("tokens in/out: {}/{}", run.input_tokens, run.output_tokens);
        std::fs::remove_dir_all(dir).ok();
    }
}

#[cfg(test)]
mod batch {
    /// Research `WARREN_SYMBOLS` (comma-separated) in parallel into a real
    /// agent's store, as a cycle would. Costs API tokens; run with
    /// `WARREN_SYMBOLS=AAPL,MSFT cargo test research_batch -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn research_batch() {
        use std::sync::Arc;
        dotenvy::dotenv().ok();
        let symbols = std::env::var("WARREN_SYMBOLS").expect("set WARREN_SYMBOLS");
        // The agent `WARREN_AGENT` names, or the first.
        let root = crate::store::Store::default_dir().unwrap();
        let strategies = crate::strategy::Strategies::new(&root);
        let list = strategies.list().unwrap();
        let strategy = match std::env::var("WARREN_AGENT") {
            Ok(id) => list
                .into_iter()
                .find(|s| s.id == id)
                .expect("no such agent"),
            Err(_) => list
                .into_iter()
                .next()
                .expect("no agents; launch Warren first"),
        };
        let global = crate::config::Config::load_or_create().unwrap();
        let (ctx, _events) = crate::agents::AgentCtx::new(
            Arc::new(crate::market::alpaca::AlpacaClient::from_env().unwrap()),
            Arc::new(crate::store::Store::open(strategies.dir(&strategy.id)).unwrap()),
            Arc::new(strategy.config(&global)),
            strategy,
        );
        let runs = symbols.split(',').map(|s| super::run(&ctx, s));
        let results = crate::runtime::RUNTIME.block_on(futures::future::join_all(runs));
        for result in results {
            match result {
                Ok(r) => {
                    let scores: Vec<String> = r
                        .factors
                        .named()
                        .iter()
                        .map(|(name, f)| format!("{name}={}", f.score))
                        .collect();
                    println!(
                        "{:5} r/r {:>5}  {}  | jev {}",
                        r.symbol,
                        r.risk_reward.map_or("-".into(), |rr| format!("{rr:.2}")),
                        scores.join(" "),
                        r.shadow.as_ref().map_or("-".into(), |s| format!(
                            "catalyst={:.2} (conf {:.2}) news={:.2} (conf {:.2})",
                            s.catalyst.score,
                            s.catalyst.confidence,
                            s.news_sentiment.score,
                            s.news_sentiment.confidence
                        ))
                    );
                }
                Err(err) => println!("ERROR {err:#}"),
            }
        }
    }
}
