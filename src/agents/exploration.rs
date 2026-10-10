//! The exploration agent: scans the market for promising swing-trade
//! candidates for the research agent to dig into.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rig_core::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AgentCtx, AgentKind, tools::*};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct Candidate {
    pub symbol: String,
    /// Why this name deserves in-depth research now, citing the data.
    pub rationale: String,
    /// 1 (research first) to 5 (research last).
    pub priority: u8,
}

/// What the model produces.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct Exploration {
    /// Two or three sentences on today's market tone and where the
    /// opportunities seem to be.
    pub market_summary: String,
    pub candidates: Vec<Candidate>,
}

/// A stored exploration result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateList {
    pub generated_at: DateTime<Utc>,
    pub market_summary: String,
    /// Highest priority first.
    pub candidates: Vec<Candidate>,
}

fn preamble(ctx: &AgentCtx) -> String {
    let config = &ctx.config.exploration;
    format!(
        "\
You are the exploration analyst for Warren, an autonomous swing-trading system \
(holding periods of days to a few weeks, US equities, long-biased). Your job is \
to find the best few stocks for a research analyst to study in depth today. You \
do not trade.

Process:
1. screen_market for today's most active names, gainers, and losers.
2. get_portfolio and list_research to see what is already held or researched \
   recently; prefer names not researched in the last day unless something changed.
3. For promising names: get_asset (exclude OTC, warrants, units, rights, \
   inactive or untradable listings), get_technicals, and get_news.
4. Optionally check SPY, QQQ, or sector ETFs with get_quotes for context.

Selection rules:
- At most {max} candidates, ordered by priority.
- Price at least ${min_price:.2} and 20-day average dollar volume at least \
  ${min_dv:.0}.
- Favor setups with a clear, current reason to act: a catalyst, a breakout or \
  pullback to support in a trend, or relative strength. Avoid one-day \
  spikes with no follow-through story, and avoid chasing extended moves.
- Each rationale must cite specific data (price levels, indicator values, \
  headlines).",
        max = config.max_candidates,
        min_price = config.min_price,
        min_dv = config.min_avg_dollar_volume,
    )
}

/// Find candidates, store them, and return them.
pub async fn run(ctx: &AgentCtx) -> Result<CandidateList> {
    let config = &ctx.config.exploration;
    ctx.recorded(AgentKind::Exploration, None, async |ctx| {
        let agent = ctx
            .builder(AgentKind::Exploration, None, &config.agent, preamble(ctx))?
            .tool(ScreenMarket(ctx.clone()))
            .tool(GetPortfolio(ctx.clone()))
            .tool(ListResearch(ctx.clone()))
            .tool(GetAsset(ctx.clone()))
            .tool(GetQuotes(ctx.clone()))
            .tool(GetTechnicals(ctx.clone()))
            .tool(GetNews(ctx.clone()))
            .build();

        let response = agent
            .prompt_typed::<Exploration>(format!(
                "Find today's best research candidates ({}).",
                Utc::now().format("%Y-%m-%d %H:%M UTC")
            ))
            .max_turns(config.agent.max_turns)
            .retries(1)
            .await
            .map_err(|err| anyhow!("exploration failed: {err}"))?;

        let mut exploration = response.output;
        for candidate in &mut exploration.candidates {
            candidate.symbol = candidate.symbol.trim().to_uppercase();
            candidate.priority = candidate.priority.clamp(1, 5);
        }
        exploration.candidates.sort_by_key(|c| c.priority);
        exploration.candidates.truncate(config.max_candidates);

        let list = CandidateList {
            generated_at: Utc::now(),
            market_summary: exploration.market_summary,
            candidates: exploration.candidates,
        };
        ctx.store.save_candidates(&list)?;
        let summary = format!(
            "{} candidates: {}",
            list.candidates.len(),
            list.candidates
                .iter()
                .map(|c| c.symbol.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok((list, summary))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exploration_schema_has_candidates() {
        let schema = serde_json::to_value(rig_core::schemars::schema_for!(Exploration)).unwrap();
        assert!(schema["properties"]["candidates"].is_object(), "{schema}");
    }
}
