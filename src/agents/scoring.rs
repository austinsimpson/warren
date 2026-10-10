//! Factor scores: context for the executor, which makes every trading
//! decision itself, and the measurements the scorecard tracks. Nothing here
//! gates a trade.
//!
//! Factors are scored on a 1–5 bullishness scale (1 = very bearish, 3 =
//! neutral, 5 = very bullish):
//! - trend, momentum, relative strength, and extension are computed from
//!   price data by [`quant_factors`];
//! - catalyst, news sentiment, and setup quality are judged by the research
//!   agent ([`JudgedFactors`]);
//! - risk/reward is derived from the agent's entry, target, and stop, and the
//!   price now.
//!
//! What the evidence says, for the executor's context: in backtests over
//! 2017–2026 and in point-in-time replays of the judged factors, not buying
//! stretched prices (extension) held up most consistently; trend, momentum,
//! and relative strength showed no edge at swing horizons, and the
//! strongest recent movers tended to reverse.

use rig_core::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::research::ResearchFindings;
use super::technicals::Technicals;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct FactorScore {
    /// 1 (very bearish) to 5 (very bullish); 3 is neutral.
    pub score: u8,
    /// One sentence citing the data behind the score.
    pub reason: String,
}

impl FactorScore {
    fn new(score: u8, reason: String) -> Self {
        Self {
            score: score.clamp(1, 5),
            reason,
        }
    }
}

/// The factors the research agent judges, each on its own evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rig_core::schemars")]
pub struct JudgedFactors {
    /// Strength and timing of a specific upcoming or recent catalyst within
    /// the horizon. 3 if there is none.
    pub catalyst: FactorScore,
    /// Tone and substance of the last two weeks of news.
    pub news_sentiment: FactorScore,
    /// Quality of the chart structure for an entry: a clear support or
    /// breakout level nearby, a well-defined base, and a logical place for
    /// the stop. (How stretched the price is is computed separately.)
    pub setup_quality: FactorScore,
}

/// Factors computed from price data. `None` when there is too little history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuantFactors {
    pub trend: Option<FactorScore>,
    pub momentum: Option<FactorScore>,
    pub relative_strength: Option<FactorScore>,
    /// How stretched the price is: high means a calm entry, low means chasing.
    pub extension: Option<FactorScore>,
}

/// Every factor behind a report, as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorScores {
    #[serde(flatten)]
    pub quant: QuantFactors,
    #[serde(flatten)]
    pub judged: JudgedFactors,
    /// Reward-to-risk mapped onto the factor scale, if a long setup.
    pub risk_reward: Option<FactorScore>,
}

impl FactorScores {
    /// Every available factor in display order.
    pub fn named(&self) -> Vec<(&'static str, &FactorScore)> {
        [
            ("trend", self.quant.trend.as_ref()),
            ("momentum", self.quant.momentum.as_ref()),
            ("rel strength", self.quant.relative_strength.as_ref()),
            ("extension", self.quant.extension.as_ref()),
            ("catalyst", Some(&self.judged.catalyst)),
            ("news", Some(&self.judged.news_sentiment)),
            ("setup", Some(&self.judged.setup_quality)),
            ("r/r", self.risk_reward.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, factor)| factor.map(|f| (name, f)))
        .collect()
    }
}

/// Combine computed and judged factors, with risk/reward from the levels.
/// `price_now` is the latest trade, used for risk/reward when the price has
/// run above the entry range. Returns the reward-to-risk ratio and every
/// factor.
pub fn score(
    findings: &ResearchFindings,
    quant: QuantFactors,
    price_now: Option<f64>,
) -> (Option<f64>, FactorScores) {
    let mut judged = findings.factors.clone();
    for factor in [
        &mut judged.catalyst,
        &mut judged.news_sentiment,
        &mut judged.setup_quality,
    ] {
        factor.score = factor.score.clamp(1, 5);
    }
    let ratio = risk_reward(findings, price_now);
    let factors = FactorScores {
        quant,
        judged,
        risk_reward: ratio.map(|ratio| {
            FactorScore::new(
                risk_reward_to_score(ratio),
                format!("{ratio:.2}:1 reward to risk from the expected fill"),
            )
        }),
    };
    (ratio, factors)
}

/// (target − entry) / (entry − stop) at the expected fill: the middle of the
/// entry range, or the top of it when the price is already above the range
/// (buys are capped at `entry_high`; see `executor::check_risk`). `None`
/// unless all levels are present and form a long setup.
fn risk_reward(f: &ResearchFindings, price_now: Option<f64>) -> Option<f64> {
    let entry = match (f.entry_low, f.entry_high) {
        (Some(lo), Some(hi)) => (lo + hi) / 2.0,
        (Some(price), None) | (None, Some(price)) => price,
        (None, None) => return None,
    };
    let entry = match (price_now, f.entry_high) {
        (Some(now), Some(high)) if now > entry => now.min(high),
        _ => entry,
    };
    let (target, stop) = (f.target_price?, f.stop_loss?);
    (target > entry && entry > stop).then(|| (target - entry) / (entry - stop))
}

fn risk_reward_to_score(ratio: f64) -> u8 {
    match ratio {
        r if r < 1.0 => 1,
        r if r < 1.5 => 2,
        r if r < 2.0 => 3,
        r if r < 3.0 => 4,
        _ => 5,
    }
}

// ---------------------------------------------------------------------------
// Computed factors. The thresholds below are judgment calls, kept together so
// they are easy to review and tune.
// ---------------------------------------------------------------------------

/// Score trend, momentum, and relative strength from daily-bar technicals for
/// the stock and, for relative strength, the benchmark.
pub fn quant_factors(stock: &Technicals, benchmark: Option<&Technicals>) -> QuantFactors {
    QuantFactors {
        trend: trend(stock),
        momentum: momentum(stock),
        relative_strength: benchmark.and_then(|b| relative_strength(stock, b)),
        extension: extension(stock),
    }
}

/// Distance above the 20-day SMA in ATRs: under 1 is a calm entry, 3 or more
/// is chasing. A week up 15% or more caps the score at 2 however far the SMA
/// has caught up. Below the SMA counts as unstretched; weakness there is the
/// trend and momentum factors' business.
fn extension(t: &Technicals) -> Option<FactorScore> {
    let (sma, atr) = (t.sma_20?, t.atr_14?);
    if atr <= 0.0 {
        return None;
    }
    let atrs_above = (t.last_close - sma) / atr;
    let mut score = match atrs_above {
        a if a < 1.0 => 5,
        a if a < 1.5 => 4,
        a if a < 2.0 => 3,
        a if a < 3.0 => 2,
        _ => 1,
    };
    let week = t.return_1w_percent.unwrap_or(0.0);
    let hot_week = week >= 15.0;
    if hot_week {
        score = score.min(2);
    }
    let range = t.high_20d - t.low_20d;
    let range_position = if range > 0.0 {
        format!(
            ", at {:.0}% of the 20-day range",
            (t.last_close - t.low_20d) / range * 100.0
        )
    } else {
        String::new()
    };
    Some(FactorScore::new(
        score,
        format!(
            "{atrs_above:+.1} ATR from SMA20 {sma:.2} (ATR {atr:.2}){range_position}, 1w {week:+.1}%{}",
            if hot_week { " (hot week)" } else { "" }
        ),
    ))
}

/// Five bullish checks: close above each of the 20/50/200-day SMAs, and the
/// SMAs stacked 20 > 50 > 200. Checks lacking history are skipped; at least
/// three must be available.
fn trend(t: &Technicals) -> Option<FactorScore> {
    let close = t.last_close;
    let checks = [
        t.sma_20
            .map(|s| (close > s, format!("close {} SMA20 {s:.2}", cmp(close > s)))),
        t.sma_50
            .map(|s| (close > s, format!("close {} SMA50 {s:.2}", cmp(close > s)))),
        t.sma_200
            .map(|s| (close > s, format!("close {} SMA200 {s:.2}", cmp(close > s)))),
        t.sma_20
            .zip(t.sma_50)
            .map(|(a, b)| (a > b, format!("SMA20 {} SMA50", cmp(a > b)))),
        t.sma_50
            .zip(t.sma_200)
            .map(|(a, b)| (a > b, format!("SMA50 {} SMA200", cmp(a > b)))),
    ];
    let available: Vec<(bool, String)> = checks.into_iter().flatten().collect();
    if available.len() < 3 {
        return None;
    }
    let bullish = available.iter().filter(|(ok, _)| *ok).count();
    let fraction = bullish as f64 / available.len() as f64;
    let score = match fraction {
        f if f >= 1.0 => 5,
        f if f >= 0.8 => 4,
        f if f >= 0.4 => 3,
        f if f > 0.0 => 2,
        _ => 1,
    };
    let details: Vec<String> = available.into_iter().map(|(_, text)| text).collect();
    Some(FactorScore::new(
        score,
        format!(
            "{bullish}/{} bullish checks at close {close:.2}: {}",
            details.len(),
            details.join(", ")
        ),
    ))
}

fn cmp(above: bool) -> &'static str {
    if above { ">" } else { "<=" }
}

/// RSI band sets the base; a strong or weak month confirmed by the week moves
/// it one step. Overbought (RSI above 70) caps the score at 3, since a swing
/// entry there is chasing.
fn momentum(t: &Technicals) -> Option<FactorScore> {
    let rsi = t.rsi_14?;
    let month = t.return_1m_percent?;
    let week = t.return_1w_percent.unwrap_or(0.0);
    let base: i32 = match rsi {
        r if r < 30.0 => 1,
        r if r < 45.0 => 2,
        r if r < 55.0 => 3,
        r if r <= 70.0 => 4,
        _ => 3,
    };
    let adjust = if month >= 5.0 && week > 0.0 {
        1
    } else if month <= -5.0 && week < 0.0 {
        -1
    } else {
        0
    };
    let mut score = (base + adjust).clamp(1, 5) as u8;
    let overbought = rsi > 70.0;
    if overbought {
        score = score.min(3);
    }
    Some(FactorScore::new(
        score,
        format!(
            "RSI14 {rsi:.1}{}, 1w {week:+.1}%, 1m {month:+.1}%",
            if overbought { " (overbought)" } else { "" }
        ),
    ))
}

/// Excess return over the benchmark, averaged across 1 and 3 months (1 month
/// alone when 3 months of history is missing).
fn relative_strength(stock: &Technicals, benchmark: &Technicals) -> Option<FactorScore> {
    let excess_1m = stock.return_1m_percent? - benchmark.return_1m_percent?;
    let excess_3m = stock
        .return_3m_percent
        .zip(benchmark.return_3m_percent)
        .map(|(s, b)| s - b);
    let excess = match excess_3m {
        Some(e3) => (excess_1m + e3) / 2.0,
        None => excess_1m,
    };
    let score = match excess {
        e if e >= 10.0 => 5,
        e if e >= 3.0 => 4,
        e if e > -3.0 => 3,
        e if e > -10.0 => 2,
        _ => 1,
    };
    Some(FactorScore::new(
        score,
        format!(
            "vs SPY: 1m {excess_1m:+.1} pts{}, average {excess:+.1} pts",
            excess_3m.map_or(String::new(), |e| format!(", 3m {e:+.1} pts"))
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factor(score: u8) -> FactorScore {
        FactorScore {
            score,
            reason: "test".into(),
        }
    }

    fn findings(
        judged: [u8; 3],
        levels: (Option<f64>, Option<f64>, Option<f64>),
    ) -> ResearchFindings {
        let [catalyst, news, setup] = judged.map(factor);
        ResearchFindings {
            horizon_days: 10,
            thesis: String::new(),
            catalysts: vec![],
            risks: vec![],
            entry_low: levels.0,
            entry_high: levels.0,
            target_price: levels.1,
            stop_loss: levels.2,
            technical_summary: String::new(),
            news_summary: String::new(),
            news_highlights: vec![],
            factors: JudgedFactors {
                catalyst,
                news_sentiment: news,
                setup_quality: setup,
            },
        }
    }

    fn quant(scores: [u8; 3]) -> QuantFactors {
        let [trend, momentum, rs] = scores.map(|s| Some(factor(s)));
        QuantFactors {
            trend,
            momentum,
            relative_strength: rs,
            extension: None,
        }
    }

    fn technicals() -> Technicals {
        Technicals {
            last_close: 100.0,
            sma_20: Some(95.0),
            sma_50: Some(90.0),
            sma_200: Some(80.0),
            rsi_14: Some(60.0),
            atr_14: Some(2.0),
            atr_percent: Some(2.0),
            return_1w_percent: Some(1.0),
            return_1m_percent: Some(6.0),
            return_3m_percent: Some(12.0),
            return_1y_percent: Some(30.0),
            return_12_1_percent: Some(25.0),
            volatility_3m_percent: Some(25.0),
            high_52w: 101.0,
            low_52w: 70.0,
            high_20d: 101.0,
            low_20d: 92.0,
            percent_below_52w_high: 1.0,
            avg_dollar_volume_20d: Some(1e8),
            bars_used: 300,
        }
    }

    #[test]
    fn every_factor_is_reported() {
        let (ratio, factors) = score(
            &findings([4; 3], (Some(100.0), Some(130.0), Some(90.0))),
            quant([4; 3]),
            None,
        );
        assert_eq!(ratio, Some(3.0));
        assert_eq!(factors.risk_reward.as_ref().unwrap().score, 5);
        // Three computed (no extension here), three judged, and r/r.
        assert_eq!(factors.named().len(), 7);
    }

    #[test]
    fn missing_factors_are_left_out() {
        let (ratio, factors) = score(
            &findings([5; 3], (None, None, None)),
            QuantFactors::default(),
            None,
        );
        assert_eq!(ratio, None);
        assert_eq!(factors.named().len(), 3);
    }

    #[test]
    fn judged_scores_are_clamped() {
        let (_, factors) = score(
            &findings([9, 0, 3], (None, None, None)),
            quant([3; 3]),
            None,
        );
        assert_eq!(factors.judged.catalyst.score, 5);
        assert_eq!(factors.judged.news_sentiment.score, 1);
    }

    #[test]
    fn trend_counts_bullish_checks() {
        assert_eq!(trend(&technicals()).unwrap().score, 5);

        let mut t = technicals();
        t.last_close = 92.0; // below SMA20 only: 4/5
        assert_eq!(trend(&t).unwrap().score, 4);

        t.last_close = 70.0; // below all three; stack still bullish: 2/5
        assert_eq!(trend(&t).unwrap().score, 3);

        t.sma_20 = Some(75.0); // SMA20 < SMA50 too: 1/5
        assert_eq!(trend(&t).unwrap().score, 2);
    }

    #[test]
    fn trend_needs_three_checks() {
        let mut t = technicals();
        t.sma_50 = None;
        t.sma_200 = None;
        assert!(trend(&t).is_none());
        t.sma_50 = Some(90.0); // close>20, close>50, 20>50
        assert_eq!(trend(&t).unwrap().score, 5);
    }

    #[test]
    fn momentum_rules() {
        // RSI 60 → 4; +6% month with an up week → 5.
        assert_eq!(momentum(&technicals()).unwrap().score, 5);

        let mut t = technicals();
        t.rsi_14 = Some(75.0); // overbought caps at 3
        assert_eq!(momentum(&t).unwrap().score, 3);

        t.rsi_14 = Some(40.0);
        t.return_1m_percent = Some(-8.0);
        t.return_1w_percent = Some(-2.0); // 2 − 1
        assert_eq!(momentum(&t).unwrap().score, 1);

        t.rsi_14 = None;
        assert!(momentum(&t).is_none());
    }

    #[test]
    fn relative_strength_averages_excess_returns() {
        let mut spy = technicals();
        spy.return_1m_percent = Some(2.0);
        spy.return_3m_percent = Some(4.0);
        // Excess +4 (1m) and +8 (3m) → +6 → 4.
        assert_eq!(relative_strength(&technicals(), &spy).unwrap().score, 4);

        let mut stock = technicals();
        stock.return_3m_percent = None; // 1m only: +4 → 4
        assert_eq!(relative_strength(&stock, &spy).unwrap().score, 4);

        stock.return_1m_percent = Some(-15.0); // −17 → 1
        assert_eq!(relative_strength(&stock, &spy).unwrap().score, 1);
    }

    #[test]
    fn extension_scores_distance_from_sma20_in_atrs() {
        // Close 100, SMA20 95, ATR 2: +2.5 ATR → 2.
        assert_eq!(extension(&technicals()).unwrap().score, 2);

        let mut t = technicals();
        t.last_close = 96.0; // +0.5 ATR
        assert_eq!(extension(&t).unwrap().score, 5);
        t.last_close = 90.0; // below the SMA is not stretched
        assert_eq!(extension(&t).unwrap().score, 5);
        t.last_close = 102.0; // +3.5 ATR
        assert_eq!(extension(&t).unwrap().score, 1);
    }

    #[test]
    fn hot_week_caps_extension() {
        let mut t = technicals();
        t.last_close = 96.0; // calm vs the SMA…
        t.return_1w_percent = Some(18.0); // …but up 18% this week
        let factor = extension(&t).unwrap();
        assert_eq!(factor.score, 2);
        assert!(factor.reason.contains("hot week"), "{}", factor.reason);
    }

    #[test]
    fn risk_reward_uses_capped_fill_when_price_ran_past_entry() {
        // Entry 100 (range 100–100), target 130, stop 90.
        let f = findings([4; 3], (Some(100.0), Some(130.0), Some(90.0)));
        let (at_entry, _) = score(&f, quant([4; 3]), Some(99.0));
        assert_eq!(at_entry, Some(3.0));

        // Range 100–104 and price 110: fill capped at 104 → 26/14.
        let mut f = f;
        f.entry_high = Some(104.0);
        let (ran, _) = score(&f, quant([4; 3]), Some(110.0));
        assert!((ran.unwrap() - 26.0 / 14.0).abs() < 1e-12);
    }
}
