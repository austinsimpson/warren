//! How Warren is doing, from its own records:
//! - **usage**: tokens and dollars spent, from the run log;
//! - **performance**: account equity against SPY since the first trade, and
//!   each filled buy against SPY since the decision;
//! - **factors**: whether each research factor predicted what the stock did
//!   next. This is the forward version of `backtest`, and the only honest
//!   test of the factors the models judge, since replaying them on past dates
//!   would leak what the models already know about those dates.
//!
//! `warren scorecard` prints it; the UI shows it in the bottom panel.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};

use crate::agents::{AgentKind, model_cost, research::ResearchReport, tools::BENCHMARK};
use crate::config::Config;
use crate::market::alpaca::{AlpacaClient, Bar, OrderSide};
use crate::store::{JournalEntry, RunRecord, Store};

/// Trading days after a report at which its factors are checked.
pub const HORIZONS: [usize; 3] = [5, 10, 21];
/// Fewer matured reports than this and a factor's correlation is noise.
pub const MIN_SAMPLE: usize = 20;

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub runs: usize,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub jev_input_tokens: u64,
    pub cost_usd: f64,
    /// Runs logged before costs were recorded, priced here from their token
    /// counts as if nothing was cached.
    pub estimated_runs: usize,
}

impl UsageTotals {
    fn add(&mut self, run: &RunRecord, fallback_model: &str) {
        self.runs += 1;
        self.input_tokens += run.input_tokens;
        self.cached_input_tokens += run.cached_input_tokens;
        self.output_tokens += run.output_tokens;
        self.jev_input_tokens += run.jev_input_tokens;
        match run.cost_usd {
            Some(cost) => self.cost_usd += cost,
            None => {
                let usage = rig_core::completion::Usage::new()
                    .input_tokens(run.input_tokens)
                    .output_tokens(run.output_tokens);
                let model = run.model.as_deref().unwrap_or(fallback_model);
                self.cost_usd += model_cost(model, &usage).unwrap_or(0.0);
                self.estimated_runs += 1;
            }
        }
    }

    pub fn cost_per_run(&self) -> Option<f64> {
        (self.runs > 0).then(|| self.cost_usd / self.runs as f64)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub today: UsageTotals,
    pub week: UsageTotals,
    pub all: UsageTotals,
    pub by_agent: Vec<(AgentKind, UsageTotals)>,
}

/// Usage totals for today (local time), the last 7 days, and all time.
pub fn usage(runs: &[RunRecord], config: &Config, now: DateTime<Utc>) -> Usage {
    let today = now.with_timezone(&Local).date_naive();
    let week_ago = now - Duration::days(7);
    let mut usage = Usage {
        by_agent: [
            AgentKind::Exploration,
            AgentKind::Research,
            AgentKind::Executor,
        ]
        .into_iter()
        .map(|kind| (kind, UsageTotals::default()))
        .collect(),
        ..Usage::default()
    };
    for run in runs {
        let fallback = match run.agent {
            AgentKind::Exploration => &config.exploration.agent.model,
            AgentKind::Research => &config.research.agent.model,
            AgentKind::Executor => &config.executor.agent.model,
        };
        usage.all.add(run, fallback);
        if run.started_at >= week_ago {
            usage.week.add(run, fallback);
        }
        if run.started_at.with_timezone(&Local).date_naive() == today {
            usage.today.add(run, fallback);
        }
        if let Some((_, totals)) = usage.by_agent.iter_mut().find(|(k, _)| *k == run.agent) {
            totals.add(run, fallback);
        }
    }
    usage
}

// ---------------------------------------------------------------------------
// Performance
// ---------------------------------------------------------------------------

/// Account equity against buying SPY instead, since the first trade.
#[derive(Debug, Clone, PartialEq)]
pub struct Performance {
    pub since: NaiveDate,
    pub warren: f64,
    pub spy: f64,
}

/// Filled buys, each measured from its fill to now against SPY over the
/// same span, whether or not the position is still held: this grades the
/// decision to buy, not the exit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decisions {
    pub filled_buys: usize,
    pub mean_return: Option<f64>,
    pub mean_excess: Option<f64>,
    pub beat_spy: usize,
}

fn decisions(
    journal: &[JournalEntry],
    fills: &HashMap<String, f64>,
    prices: &HashMap<String, f64>,
) -> Decisions {
    let mut out = Decisions::default();
    let (mut returns, mut excesses) = (Vec::new(), Vec::new());
    for entry in journal.iter().filter(|e| e.side == OrderSide::Buy) {
        let (Some(fill), Some(spy_then)) = (
            fills.get(&entry.order_id),
            entry.benchmark_price_at_decision,
        ) else {
            continue;
        };
        let (Some(now), Some(spy_now)) = (prices.get(&entry.symbol), prices.get(BENCHMARK)) else {
            continue;
        };
        let ret = now / fill - 1.0;
        let excess = ret - (spy_now / spy_then - 1.0);
        out.filled_buys += 1;
        out.beat_spy += usize::from(excess > 0.0);
        returns.push(ret);
        excesses.push(excess);
    }
    out.mean_return = mean(&returns);
    out.mean_excess = mean(&excesses);
    out
}

// ---------------------------------------------------------------------------
// Factor study
// ---------------------------------------------------------------------------

/// One factor's record at one horizon.
#[derive(Debug, Clone, PartialEq)]
pub struct FactorStat {
    pub name: &'static str,
    /// Matured reports with this factor.
    pub n: usize,
    /// Spearman rank correlation between the score and the return above
    /// SPY; the "information coefficient". Above about 0.05 is useful in
    /// practice, if it holds up.
    pub ic: Option<f64>,
    /// Mean return above SPY of high scores minus low scores.
    pub spread: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HorizonStudy {
    pub days: usize,
    pub matured: usize,
    pub factors: Vec<FactorStat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FactorStudy {
    /// Reports counted: the latest per symbol per trading day.
    pub reports: usize,
    pub horizons: Vec<HorizonStudy>,
    /// Roughly when the next report matures at the shortest horizon.
    pub next_maturity: Option<NaiveDate>,
}

/// A factor's value on a report, and its high and low cutoffs.
struct FactorDef {
    name: &'static str,
    value: fn(&ResearchReport) -> Option<f64>,
    high: f64,
    low: f64,
}

fn score(report: &ResearchReport, name: &str) -> Option<f64> {
    report
        .factors
        .named()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f64::from(f.score))
}

const FACTOR_DEFS: [FactorDef; 13] = [
    FactorDef {
        name: "conviction",
        value: |r| Some(r.conviction),
        high: 0.6,
        low: 0.5,
    },
    FactorDef {
        name: "trend",
        value: |r| score(r, "trend"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "momentum",
        value: |r| score(r, "momentum"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "rel strength",
        value: |r| score(r, "rel strength"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "extension",
        value: |r| score(r, "extension"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "catalyst",
        value: |r| score(r, "catalyst"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "news",
        value: |r| score(r, "news"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "setup",
        value: |r| score(r, "setup"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "r/r",
        value: |r| score(r, "r/r"),
        high: 4.0,
        low: 2.0,
    },
    FactorDef {
        name: "12-1m return",
        value: |r| r.trend_health?.return_12_1_percent,
        high: 20.0,
        low: -20.0,
    },
    FactorDef {
        // Negated, so higher is closer to the high like the other factors.
        name: "near 52w high",
        value: |r| r.trend_health.map(|h| -h.percent_below_52w_high),
        high: -10.0,
        low: -45.0,
    },
    FactorDef {
        name: "jev catalyst",
        value: |r| r.shadow.as_ref().map(|s| s.catalyst.score),
        high: 3.5,
        low: 2.5,
    },
    FactorDef {
        name: "jev news",
        value: |r| r.shadow.as_ref().map(|s| s.news_sentiment.score),
        high: 3.5,
        low: 2.5,
    },
];

/// A daily bar's session is over (with the 16-minute SIP delay) once this
/// long has passed since its timestamp, which is midnight New York time.
const SESSION_END: Duration = Duration::minutes(16 * 60 + 20);

/// The index of the session a report belongs to: the last bar at or before
/// it. A report written during a session is measured from that session's
/// close.
fn session_of(bars: &[Bar], at: DateTime<Utc>) -> Option<usize> {
    bars.partition_point(|b| b.timestamp <= at).checked_sub(1)
}

/// Return of `bars` from session `start`'s close to `days` sessions later,
/// if that session has closed.
fn forward(bars: &[Bar], start: usize, days: usize, now: DateTime<Utc>) -> Option<f64> {
    let end = bars.get(start + days)?;
    (end.timestamp + SESSION_END <= now).then(|| end.close / bars[start].close - 1.0)
}

/// The study over `reports`, given daily bars for every symbol and SPY.
fn study(
    reports: &[ResearchReport],
    bars: &HashMap<String, Vec<Bar>>,
    spy: &[Bar],
    now: DateTime<Utc>,
) -> FactorStudy {
    // Latest report per symbol per session: same-day re-runs aren't new
    // evidence.
    let mut latest: HashMap<(String, NaiveDate), &ResearchReport> = HashMap::new();
    for report in reports {
        let key = (report.symbol.clone(), report.generated_at.date_naive());
        let keep = latest
            .get(&key)
            .is_none_or(|kept| kept.generated_at < report.generated_at);
        if keep {
            latest.insert(key, report);
        }
    }
    let reports: Vec<&ResearchReport> = latest.into_values().collect();

    let mut next_maturity: Option<NaiveDate> = None;
    let horizons = HORIZONS
        .iter()
        .map(|&days| {
            // (report, return above SPY) for every matured report.
            let matured: Vec<(&ResearchReport, f64)> = reports
                .iter()
                .filter_map(|report| {
                    let stock = bars.get(&report.symbol)?;
                    let (i, j) = (
                        session_of(stock, report.generated_at)?,
                        session_of(spy, report.generated_at)?,
                    );
                    match (forward(stock, i, days, now), forward(spy, j, days, now)) {
                        (Some(stock), Some(spy)) => Some((*report, stock - spy)),
                        _ => {
                            if days == HORIZONS[0] {
                                // About 7 calendar days per 5 sessions.
                                let due = (stock[i].timestamp
                                    + Duration::days((days * 7).div_ceil(5) as i64))
                                .date_naive();
                                next_maturity = Some(next_maturity.map_or(due, |d| d.min(due)));
                            }
                            None
                        }
                    }
                })
                .collect();
            let factors = FACTOR_DEFS
                .iter()
                .map(|def| {
                    let pairs: Vec<(f64, f64)> = matured
                        .iter()
                        .filter_map(|(r, excess)| Some(((def.value)(r)?, *excess)))
                        .collect();
                    let high: Vec<f64> = pairs
                        .iter()
                        .filter(|(v, _)| *v >= def.high)
                        .map(|p| p.1)
                        .collect();
                    let low: Vec<f64> = pairs
                        .iter()
                        .filter(|(v, _)| *v <= def.low)
                        .map(|p| p.1)
                        .collect();
                    FactorStat {
                        name: def.name,
                        n: pairs.len(),
                        ic: spearman(&pairs),
                        spread: mean(&high).zip(mean(&low)).map(|(h, l)| h - l),
                    }
                })
                .collect();
            HorizonStudy {
                days,
                matured: matured.len(),
                factors,
            }
        })
        .collect();
    FactorStudy {
        reports: reports.len(),
        horizons,
        next_maturity,
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// Ranks starting at 1, ties sharing their average rank.
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && values[order[j + 1]] == values[order[i]] {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for &k in &order[i..=j] {
            ranks[k] = rank;
        }
        i = j + 1;
    }
    ranks
}

/// Spearman correlation: Pearson on the ranks. `None` under 3 pairs or when
/// either side doesn't vary.
fn spearman(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.len() < 3 {
        return None;
    }
    let x = ranks(&pairs.iter().map(|p| p.0).collect::<Vec<_>>());
    let y = ranks(&pairs.iter().map(|p| p.1).collect::<Vec<_>>());
    let (mx, my) = (mean(&x)?, mean(&y)?);
    let cov: f64 = x.iter().zip(&y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let vx: f64 = x.iter().map(|a| (a - mx).powi(2)).sum();
    let vy: f64 = y.iter().map(|b| (b - my).powi(2)).sum();
    (vx > 0.0 && vy > 0.0).then(|| cov / (vx * vy).sqrt())
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Scorecard {
    pub computed_at: DateTime<Utc>,
    pub performance: Option<Performance>,
    pub decisions: Decisions,
    pub factors: FactorStudy,
}

/// Everything except usage, which is local and cheap; see [`usage`].
pub async fn compute(alpaca: &AlpacaClient, store: &Store) -> Result<Scorecard> {
    let now = Utc::now();
    let journal = store.journal()?;
    let reports = store.all_report_history()?;

    // Bars from shortly before the first report or trade.
    let first = reports
        .iter()
        .map(|r| r.generated_at)
        .chain(journal.first().map(|e| e.decided_at))
        .min()
        .unwrap_or(now);
    let start = first - Duration::days(10);
    let symbols: HashSet<String> = reports.iter().map(|r| r.symbol.clone()).collect();
    let mut bars = HashMap::new();
    for symbol in &symbols {
        bars.insert(symbol.clone(), alpaca.daily_history(symbol, start).await?);
    }
    let spy = alpaca.daily_history(BENCHMARK, start).await?;

    let (performance, decisions) = if journal.is_empty() {
        (None, Decisions::default())
    } else {
        let orders = alpaca.recent_orders(500).await?;
        let fills: HashMap<String, f64> = orders
            .iter()
            .filter(|o| o.filled_qty > 0.0)
            .filter_map(|o| Some((o.id.clone(), o.filled_avg_price?)))
            .collect();
        let mut wanted: Vec<String> = journal.iter().map(|e| e.symbol.clone()).collect();
        wanted.push(BENCHMARK.into());
        wanted.sort();
        wanted.dedup();
        let prices: HashMap<String, f64> = alpaca
            .snapshots(&wanted)
            .await?
            .into_iter()
            .filter_map(|(symbol, snap)| Some((symbol, snap.last_price()?)))
            .collect();
        let performance = performance(alpaca, &journal[0], &spy, &prices).await?;
        (performance, decisions(&journal, &fills, &prices))
    };

    Ok(Scorecard {
        computed_at: now,
        performance,
        decisions,
        factors: study(&reports, &bars, &spy, now),
    })
}

/// From the close before the first decision to now.
async fn performance(
    alpaca: &AlpacaClient,
    first: &JournalEntry,
    spy: &[Bar],
    prices: &HashMap<String, f64>,
) -> Result<Option<Performance>> {
    let equity = alpaca
        .equity_history(first.decided_at - Duration::days(10))
        .await?;
    let decided = first.decided_at.date_naive();
    let Some(&(base_at, base)) = equity
        .iter()
        .rev()
        .find(|(at, _)| at.date_naive() < decided)
        .or(equity.first())
    else {
        return Ok(None);
    };
    let Some(spy_base) = spy
        .iter()
        .rev()
        .find(|b| b.timestamp.date_naive() <= base_at.date_naive())
    else {
        return Ok(None);
    };
    let account = alpaca.account().await.context("fetching account")?;
    let Some(spy_now) = prices.get(BENCHMARK) else {
        return Ok(None);
    };
    Ok(Some(Performance {
        since: base_at.date_naive(),
        warren: account.equity / base - 1.0,
        spy: spy_now / spy_base.close - 1.0,
    }))
}

/// Entry point for `warren scorecard`.
pub fn run_cli() -> Result<()> {
    let config = Config::load_or_create()?;
    let store = Store::open_default()?;
    let alpaca = AlpacaClient::from_env()?;
    let usage = usage(&store.runs()?, &config, Utc::now());
    let card = crate::runtime::RUNTIME.block_on(compute(&alpaca, &store))?;
    print!("{}", render_text(&usage, &card));
    Ok(())
}

fn pct(x: f64) -> String {
    format!("{:+.2}%", x * 100.0)
}

pub fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn render_text(usage: &Usage, card: &Scorecard) -> String {
    let mut out =
        String::from("usage            runs     input   cached   output      jev      cost\n");
    let row = |label: &str, t: &UsageTotals| {
        format!(
            "  {label:<12} {:>6} {:>9} {:>8} {:>8} {:>8} {:>9}\n",
            t.runs,
            tokens(t.input_tokens),
            tokens(t.cached_input_tokens),
            tokens(t.output_tokens),
            tokens(t.jev_input_tokens),
            format!("${:.2}", t.cost_usd),
        )
    };
    out += &row("today", &usage.today);
    out += &row("7 days", &usage.week);
    out += &row("all time", &usage.all);
    for (kind, totals) in &usage.by_agent {
        out += &row(&format!("  {kind}"), totals);
    }
    if usage.all.estimated_runs > 0 {
        out += &format!(
            "  ({} older runs priced from token counts, assuming no caching)\n",
            usage.all.estimated_runs
        );
    }

    out += "\nperformance\n";
    match &card.performance {
        Some(p) => {
            out += &format!(
                "  since {}: Warren {}, SPY {}, difference {}\n",
                p.since,
                pct(p.warren),
                pct(p.spy),
                pct(p.warren - p.spy)
            )
        }
        None => out += "  no trades yet\n",
    }
    let d = &card.decisions;
    if d.filled_buys > 0 {
        out += &format!(
            "  {} filled buys: mean {}, mean vs SPY {}, {} beat SPY\n",
            d.filled_buys,
            pct(d.mean_return.unwrap_or(0.0)),
            pct(d.mean_excess.unwrap_or(0.0)),
            d.beat_spy
        );
    }

    let f = &card.factors;
    out += &format!(
        "\nfactors: {} reports (latest per symbol per day); IC is the rank correlation \
         with the\nreturn above SPY, spread is high minus low scores. Under {MIN_SAMPLE} \
         matured reports, read nothing into it.\n",
        f.reports
    );
    if let Some(due) = f.next_maturity {
        out += &format!("  next report matures around {due}\n");
    }
    out += &format!("  {:<14}", "");
    for h in &f.horizons {
        out += &format!("{:>22}", format!("{}d (n {})", h.days, h.matured));
    }
    out += "\n";
    for (k, def) in FACTOR_DEFS.iter().enumerate() {
        out += &format!("  {:<14}", def.name);
        for h in &f.horizons {
            let stat = &h.factors[k];
            let cell = match (stat.ic, stat.spread) {
                (Some(ic), Some(spread)) => format!("IC {ic:+.2} {}", pct(spread)),
                (Some(ic), None) => format!("IC {ic:+.2}"),
                _ => "–".into(),
            };
            out += &format!("{cell:>22}");
        }
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_support::sample_report;

    fn bars_from(start: &str, closes: &[f64]) -> Vec<Bar> {
        let start: DateTime<Utc> = start.parse().unwrap();
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Bar {
                timestamp: start + Duration::days(i as i64),
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 0,
                trade_count: 0,
                vwap: c,
            })
            .collect()
    }

    #[test]
    fn spearman_handles_ties_and_direction() {
        let up = [(1.0, 0.1), (2.0, 0.2), (3.0, 0.3), (4.0, 0.4)];
        assert!((spearman(&up).unwrap() - 1.0).abs() < 1e-12);
        let down = [(1.0, 0.4), (2.0, 0.3), (3.0, 0.2), (4.0, 0.1)];
        assert!((spearman(&down).unwrap() + 1.0).abs() < 1e-12);
        assert_eq!(ranks(&[3.0, 1.0, 3.0]), vec![2.5, 1.0, 2.5]);
        assert_eq!(spearman(&[(1.0, 0.1), (1.0, 0.2), (1.0, 0.3)]), None);
    }

    #[test]
    fn reports_mature_only_after_the_session_closes() {
        // Sessions at 04:00Z on consecutive days.
        let stock = bars_from(
            "2026-10-01T04:00:00Z",
            &[10.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0],
        );
        let spy = bars_from("2026-10-01T04:00:00Z", &[100.0; 7]);
        let mut report = sample_report("AAA", crate::agents::research::SCORING_VERSION);
        // Written mid-session on the second day: measured from its close.
        report.generated_at = "2026-10-02T15:00:00Z".parse().unwrap();
        let bars = HashMap::from([("AAA".to_string(), stock)]);

        // Five sessions on is Oct 7, which closes at 20:00Z.
        let before = study(
            std::slice::from_ref(&report),
            &bars,
            &spy,
            "2026-10-07T19:00:00Z".parse().unwrap(),
        );
        assert_eq!(before.horizons[0].matured, 0);
        assert!(before.next_maturity.is_some());

        let after = study(
            std::slice::from_ref(&report),
            &bars,
            &spy,
            "2026-10-07T21:00:00Z".parse().unwrap(),
        );
        assert_eq!(after.horizons[0].matured, 1);
        assert_eq!(after.reports, 1);
    }

    #[test]
    fn same_day_reruns_count_once() {
        let stock = bars_from("2026-10-01T04:00:00Z", &[10.0; 30]);
        let spy = bars_from("2026-10-01T04:00:00Z", &[100.0; 30]);
        let mut a = sample_report("AAA", 1);
        a.generated_at = "2026-10-02T15:00:00Z".parse().unwrap();
        let mut b = a.clone();
        b.generated_at = "2026-10-02T18:00:00Z".parse().unwrap();
        let bars = HashMap::from([("AAA".to_string(), stock)]);
        let s = study(
            &[a, b],
            &bars,
            &spy,
            "2026-11-01T00:00:00Z".parse().unwrap(),
        );
        assert_eq!(s.reports, 1);
    }

    #[test]
    fn decisions_measure_fills_against_spy() {
        let mut entry = JournalEntry {
            decided_at: Utc::now(),
            symbol: "AAA".into(),
            side: OrderSide::Buy,
            qty: 1,
            order_type: crate::market::alpaca::OrderType::Market,
            limit_price: None,
            order_id: "o1".into(),
            price_at_decision: 10.0,
            benchmark_price_at_decision: Some(100.0),
            rationale: String::new(),
            research_generated_at: None,
            research_rating: None,
            research_conviction: None,
            research_factors: None,
            research_shadow: None,
        };
        let unfilled = JournalEntry {
            order_id: "o2".into(),
            ..entry.clone()
        };
        entry.order_id = "o1".into();
        let fills = HashMap::from([("o1".to_string(), 10.0)]);
        let prices = HashMap::from([("AAA".to_string(), 11.0), ("SPY".to_string(), 105.0)]);
        let d = decisions(&[entry, unfilled], &fills, &prices);
        assert_eq!(d.filled_buys, 1);
        assert!((d.mean_excess.unwrap() - 0.05).abs() < 1e-12);
        assert_eq!(d.beat_spy, 1);
    }

    #[test]
    fn usage_prices_old_runs_from_tokens() {
        let run = |cost: Option<f64>| RunRecord {
            agent: AgentKind::Research,
            subject: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            ok: true,
            summary: String::new(),
            input_tokens: 1_000_000,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            model: None,
            cost_usd: cost,
            jev_input_tokens: 0,
            jev_output_tokens: 0,
        };
        let u = usage(&[run(Some(0.5)), run(None)], &Config::default(), Utc::now());
        assert_eq!(u.all.runs, 2);
        assert_eq!(u.all.estimated_runs, 1);
        // The old run is priced at the catalog's Opus input rate.
        assert!(u.all.cost_usd > 0.5);
        assert_eq!(u.today.runs, 2);
    }
}
