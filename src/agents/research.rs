//! The research agent: an in-depth swing-trading analysis of one symbol,
//! returned as a typed [`ResearchReport`].

use std::fmt;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rig_core::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    AgentCtx, AgentKind,
    jev::ShadowScores,
    scoring::{self, FactorScores, JudgedFactors, QuantFactors},
    technicals::{self, Technicals},
    tools::*,
};
use std::future::IntoFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    StrongBuy,
    Buy,
    Hold,
    Sell,
    StrongSell,
}

impl Rating {
    pub fn is_buy(self) -> bool {
        matches!(self, Self::StrongBuy | Self::Buy)
    }
}

impl fmt::Display for Rating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StrongBuy => "strong_buy",
            Self::Buy => "buy",
            Self::Hold => "hold",
            Self::Sell => "sell",
            Self::StrongSell => "strong_sell",
        })
    }
}

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
    /// What would make the thesis wrong.
    pub risks: Vec<String>,
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

/// Bump when scoring changes so reports scored differently are not compared
/// as if they were the same scale.
///
/// - 1: every factor in conviction.
/// - 2: trend, momentum, and relative strength moved to context; see
///   [`scoring`](super::scoring). Version 1 reports are rescored on load,
///   since conviction is computed from the factors they stored.
pub const SCORING_VERSION: u32 = 2;

/// How broken a stock's long-run trend is. Among stocks like the ones
/// exploration screens, those far below their 52-week high or down over
/// the past year (skipping the last month) kept lagging; see `backtest`.
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

impl ResearchReport {
    /// Rescore a report from an older scoring version whose factors carry
    /// over, so its conviction is on today's scale. Others are unchanged.
    pub fn upgraded(mut self) -> Self {
        if self.scoring_version == 1 {
            self.conviction = scoring::conviction(&self.factors);
            self.rating = scoring::rating_for(self.conviction);
            self.scoring_version = SCORING_VERSION;
        }
        self
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
    /// The [`SCORING_VERSION`] that produced `rating` and `conviction`.
    #[serde(default)]
    pub scoring_version: u32,
    pub rating: Rating,
    /// Computed: 0 = maximally bearish, 0.5 = neutral, 1 = maximally bullish.
    pub conviction: f64,
    /// Computed reward-to-risk of the entry, target, and stop, if a long setup.
    pub risk_reward: Option<f64>,
    /// Every factor behind the conviction, computed and judged.
    pub factors: FactorScores,
    /// Long-run price health the executor's buy vetoes check; absent on
    /// reports written before it was recorded.
    #[serde(default)]
    pub trend_health: Option<TrendHealth>,
    /// Jev's scores for the same judgment factors, recorded for comparison.
    /// They do not affect conviction.
    #[serde(default)]
    pub shadow: Option<ShadowScores>,
    pub findings: ResearchFindings,
}

const PREAMBLE: &str = "\
You are the research analyst for Warren, an autonomous swing-trading system \
(holding periods of days to a few weeks, US equities, long-biased). You write \
one in-depth report on one stock. An executor agent will size and place real \
(paper) trades from your rating, conviction, entry range, target, and stop, \
so be calibrated and specific.

Process:
1. get_asset to confirm it is an active, tradable listed equity.
2. get_quotes and get_technicals for price, levels, and volatility.
3. get_price_history (daily, ~6 months) to judge structure: support and \
   resistance, recent breakouts or breakdowns, gaps.
4. get_news (about 14 days) for catalysts, earnings, guidance, and sentiment.

Scoring: you do not choose a rating or conviction. Warren has already \
computed trend, momentum, relative strength, and extension (how stretched \
the price is) from price data (shown in the request); do not re-score them, \
and keep extension out of setup quality, which is about chart structure. You score only catalyst, news \
sentiment, and setup quality, each from 1 to 5 on its own evidence, \
independently of the others and of your overall view: 1 = clearly bearish, \
2 = leaning bearish, 3 = neutral or no signal, 4 = leaning bullish, \
5 = clearly bullish. Use the full range when the data supports it; a 3 \
should mean the evidence is genuinely mixed or absent. Give each score a \
one-sentence reason citing the data. Warren computes conviction and the \
rating from your three scores, extension, and the reward-to-risk of your \
entry, target, and stop, so set those levels honestly. Trend, momentum, and \
relative strength are context: useful for reading the chart, but not part \
of conviction.

Levels:
- For a long setup, give an entry range, a target, and a stop below entry. \
  Stops should sit beyond a meaningful level, roughly 1–2.5 ATR from entry. \
  Targets should be reachable within the horizon given recent volatility.
- If you would not buy, you may leave the levels empty.

Rules:
- Ground every claim in tool data; cite indicator values and prices.
- You have no fundamentals feed; do not invent financial figures.";

/// Research `symbol`, store the report, and return it.
pub async fn run(ctx: &AgentCtx, symbol: &str) -> Result<ResearchReport> {
    let symbol = symbol.trim().to_uppercase();
    let config = &ctx.config.research.agent;
    ctx.recorded(AgentKind::Research, Some(&symbol), async |ctx| {
        let (quant, trend_health) = compute_quant_factors(ctx, &symbol).await?;
        let agent = ctx
            .builder(AgentKind::Research, Some(&symbol), config, PREAMBLE.into())?
            .tool(GetAsset(ctx.clone()))
            .tool(GetQuotes(ctx.clone()))
            .tool(GetTechnicals(ctx.clone()))
            .tool(GetPriceHistory(ctx.clone()))
            .tool(GetNews(ctx.clone()))
            .build();

        let research = agent
            .prompt_typed::<ResearchFindings>(format!(
                "Research {symbol} now ({}) and return your report.\n\n\
                 Computed factors (fixed; do not re-score):\n{}",
                Utc::now().format("%Y-%m-%d %H:%M UTC"),
                describe(&quant),
            ))
            .max_turns(config.max_turns)
            .retries(1)
            .into_future();
        let (response, shadow) = tokio::join!(research, shadow_score(ctx, &symbol));
        let response = response.map_err(|err| anyhow!("research on {symbol} failed: {err}"))?;

        let price = ctx
            .alpaca
            .snapshots(std::slice::from_ref(&symbol))
            .await
            .ok()
            .and_then(|s| s.get(&symbol).and_then(|s| s.last_price()));
        let findings = response.output;
        let (scored, factors) = scoring::score(&findings, quant, price);
        let report = ResearchReport {
            symbol: symbol.clone(),
            generated_at: Utc::now(),
            model: config.model.clone(),
            price_at_research: price,
            scoring_version: SCORING_VERSION,
            rating: scored.rating,
            conviction: scored.conviction,
            risk_reward: scored.risk_reward,
            factors,
            shadow,
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
        let summary = format!(
            "{} {} conviction {:.2} [{}]{shadow}",
            symbol,
            report.rating,
            report.conviction,
            factors.join(" ")
        );
        Ok((report, summary))
    })
    .await
}

/// Trend, momentum, and relative strength from about 18 months of daily
/// bars for the stock and the benchmark.
/// The computed factors and trend health for `symbol`.
async fn compute_quant_factors(
    ctx: &AgentCtx,
    symbol: &str,
) -> Result<(QuantFactors, TrendHealth)> {
    let start = Utc::now() - chrono::Duration::days(550);
    let (stock, benchmark) = tokio::try_join!(
        ctx.alpaca.daily_history(symbol, start),
        ctx.alpaca.daily_history(BENCHMARK, start),
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
        let since = Utc::now() - chrono::Duration::days(14);
        let news = ctx.alpaca.news(&[symbol.to_string()], since, 50).await?;
        jev.score(symbol, &news).await
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
    fn findings_schema_asks_for_factors_not_a_rating() {
        let schema =
            serde_json::to_value(rig_core::schemars::schema_for!(ResearchFindings)).unwrap();
        let properties = &schema["properties"];
        assert!(properties["factors"].is_object(), "{schema}");
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
    /// Research `WARREN_SYMBOLS` (comma-separated) in parallel into the real
    /// store, as a cycle would. Costs API tokens; run with
    /// `WARREN_SYMBOLS=AAPL,MSFT cargo test research_batch -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn research_batch() {
        use std::sync::Arc;
        dotenvy::dotenv().ok();
        let symbols = std::env::var("WARREN_SYMBOLS").expect("set WARREN_SYMBOLS");
        let (ctx, _events) = crate::agents::AgentCtx::new(
            Arc::new(crate::market::alpaca::AlpacaClient::from_env().unwrap()),
            Arc::new(crate::store::Store::open_default().unwrap()),
            Arc::new(crate::config::Config::load_or_create().unwrap()),
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
                        "{:5} {:11} conv {:.3}  r/r {:>5}  {}  | jev {}",
                        r.symbol,
                        r.rating.to_string(),
                        r.conviction,
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
