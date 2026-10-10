//! Point-in-time replay: run an agent's research as of past dates, with
//! every data tool cut off at that moment, then score the reports against
//! what happened next. This is the honest backtest of the judged factors
//! (catalyst, news, setup) and of research's setups, but only for dates after the
//! model's knowledge cutoff: before it, the model may simply remember what
//! happened.
//!
//! `warren replay [--agent ID] [--from DATE] [--to DATE] [--every DAYS]
//!  [--per-date N] [--symbols A,B,...] [--name RUN] [--concurrency N]
//!  [--yes] [--report-only] [--allow-before-cutoff]`
//!
//! Without `--yes` it prints the plan and its estimated cost. Runs resume:
//! reports already in the run are skipped. Results live in
//! `<data dir>/warren/replays/<name>/`.
//!
//! What it can't replay: exploration's picks (each date samples
//! exploration's screen instead), the executor, and Jev's own training
//! cutoff, which TypeSafe hasn't published.

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, TimeZone as _, Utc};
use futures::{StreamExt as _, stream};

use crate::agents::{AgentCtx, research, tools::BENCHMARK};
use crate::backtest::{BarCache, Screen, fetch_screened_pool};
use crate::config::Config;
use crate::market::alpaca::{AlpacaClient, Bar};
use crate::scorecard;
use crate::store::Store;
use crate::strategy::Strategies;

/// The first date after Opus 5.5's knowledge cutoff (June 2026). Earlier
/// replays need `--allow-before-cutoff` and can't be trusted.
const FIRST_CLEAN_DATE: &str = "2026-07-01";
/// Replayed moments are this long after midnight UTC on the session date:
/// after the close in both EDT and EST, so the day's bar is complete. Forward
/// returns start at the next open, so after-hours news can't help.
const REPLAY_TIME_UTC: (u32, u32) = (21, 30);
/// Cost per research run when the agent has no history to estimate from.
const FALLBACK_COST: f64 = 0.12;

struct Args {
    agent: Option<String>,
    from: NaiveDate,
    to: NaiveDate,
    every: usize,
    per_date: usize,
    symbols: Option<Vec<String>>,
    name: Option<String>,
    concurrency: usize,
    yes: bool,
    report_only: bool,
    allow_before_cutoff: bool,
}

fn parse(args: &[String]) -> Result<Args> {
    let mut parsed = Args {
        agent: None,
        from: FIRST_CLEAN_DATE.parse()?,
        // Late enough that most reports can mature at the shortest horizon.
        to: (Utc::now() - Duration::days(10)).date_naive(),
        every: 10,
        per_date: 8,
        symbols: None,
        name: None,
        concurrency: 4,
        yes: false,
        report_only: false,
        allow_before_cutoff: false,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--agent" => parsed.agent = Some(value()?),
            "--from" => parsed.from = value()?.parse().context("--from needs YYYY-MM-DD")?,
            "--to" => parsed.to = value()?.parse().context("--to needs YYYY-MM-DD")?,
            "--every" => parsed.every = value()?.parse().context("--every needs a number")?,
            "--per-date" => {
                parsed.per_date = value()?.parse().context("--per-date needs a number")?
            }
            "--symbols" => {
                parsed.symbols = Some(
                    value()?
                        .split(',')
                        .map(|s| s.trim().to_uppercase())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }
            "--name" => parsed.name = Some(value()?),
            "--concurrency" => {
                parsed.concurrency = value()?.parse().context("--concurrency needs a number")?
            }
            "--yes" => parsed.yes = true,
            "--report-only" => parsed.report_only = true,
            "--allow-before-cutoff" => parsed.allow_before_cutoff = true,
            other => bail!("unknown flag {other}"),
        }
    }
    if parsed.every == 0 || parsed.per_date == 0 || parsed.concurrency == 0 {
        bail!("--every, --per-date, and --concurrency must be at least 1");
    }
    Ok(parsed)
}

/// Entry point for `warren replay`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let args = parse(args)?;
    let clean: NaiveDate = FIRST_CLEAN_DATE.parse()?;
    if args.from < clean && !args.allow_before_cutoff {
        bail!(
            "--from {} is before {clean}, the model's knowledge cutoff: it may remember what \
             happened. Pass --allow-before-cutoff to run anyway.",
            args.from
        );
    }

    let global = Config::load_or_create()?;
    let root = Store::default_dir()?;
    let strategies = Strategies::new(&root);
    let list = strategies.list()?;
    let strategy = match &args.agent {
        Some(id) => list
            .into_iter()
            .find(|s| &s.id == id)
            .with_context(|| format!("no agent with id {id:?}"))?,
        None => list
            .into_iter()
            .next()
            .context("no agents yet; launch Warren to create one")?,
    };
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-{}", strategy.id, args.from));
    let store = Arc::new(Store::open(root.join("replays").join(&name))?);
    let alpaca = Arc::new(AlpacaClient::from_env()?);
    let runtime = &crate::runtime::RUNTIME;

    if !args.report_only {
        let plan = runtime.block_on(plan(&alpaca, &args, &strategy, &store))?;
        let per_run = scorecard::usage(
            &Store::open(strategies.dir(&strategy.id))?.runs()?,
            &global,
            Utc::now(),
        )
        .by_agent
        .into_iter()
        .find(|(kind, _)| *kind == crate::agents::AgentKind::Research)
        .and_then(|(_, t)| t.cost_per_run())
        .unwrap_or(FALLBACK_COST);
        let dates: Vec<NaiveDate> = {
            let mut d: Vec<_> = plan.iter().map(|(date, _)| *date).collect();
            d.dedup();
            d
        };
        println!(
            "replay {name:?} for agent {:?}: {} research runs over {} dates ({} to {}), \
             about ${:.2} at ${per_run:.3} a run",
            strategy.name,
            plan.len(),
            dates.len(),
            dates.first().map_or("-".into(), |d| d.to_string()),
            dates.last().map_or("-".into(), |d| d.to_string()),
            plan.len() as f64 * per_run,
        );
        for date in &dates {
            let symbols: Vec<&str> = plan
                .iter()
                .filter(|(d, _)| d == date)
                .map(|(_, s)| s.as_str())
                .collect();
            println!("  {date}: {}", symbols.join(" "));
        }
        if !args.yes {
            println!("\nnothing run; add --yes to run it");
            return Ok(());
        }
        let config = Arc::new(strategy.config(&global));
        let (ctx, _events) = AgentCtx::new(alpaca.clone(), store.clone(), config, strategy.clone());
        let total = plan.len();
        let done = runtime.block_on(
            stream::iter(plan)
                .map(|(date, symbol)| {
                    let mut ctx = ctx.clone();
                    ctx.as_of = Some(moment(date));
                    async move {
                        let result = research::run(&ctx, &symbol).await;
                        (date, symbol, result)
                    }
                })
                .buffer_unordered(args.concurrency)
                .enumerate()
                .map(|(i, (date, symbol, result))| {
                    match &result {
                        Ok(r) => println!(
                            "[{}/{total}] {date} {symbol}: {}",
                            i + 1,
                            if r.findings.entry_high.is_some() {
                                "long setup"
                            } else {
                                "no setup"
                            },
                        ),
                        Err(err) => {
                            println!("[{}/{total}] {date} {symbol}: failed: {err:#}", i + 1)
                        }
                    }
                    result.is_ok()
                })
                .collect::<Vec<bool>>(),
        );
        println!(
            "{} of {total} runs succeeded",
            done.iter().filter(|ok| **ok).count()
        );
    }

    runtime.block_on(report(&alpaca, &store))
}

/// The replayed moment for a session date.
fn moment(date: NaiveDate) -> DateTime<Utc> {
    let (h, m) = REPLAY_TIME_UTC;
    Utc.from_utc_datetime(&date.and_hms_opt(h, m, 0).expect("valid time"))
}

/// (date, symbol) pairs to research, skipping ones already in the run.
async fn plan(
    alpaca: &AlpacaClient,
    args: &Args,
    strategy: &crate::strategy::Strategy,
    store: &Store,
) -> Result<Vec<(NaiveDate, String)>> {
    let spy = alpaca
        .daily_history(BENCHMARK, Utc.from_utc_datetime(&args.from.into()))
        .await?;
    let dates: Vec<NaiveDate> = spy
        .iter()
        .map(|b| b.timestamp.date_naive())
        .filter(|d| *d >= args.from && *d <= args.to)
        .step_by(args.every)
        .collect();
    if dates.is_empty() {
        bail!("no trading days between {} and {}", args.from, args.to);
    }

    let done: Vec<(NaiveDate, String)> = store
        .all_report_history()?
        .into_iter()
        .map(|r| (r.generated_at.date_naive(), r.symbol))
        .collect();

    let mut plan = Vec::new();
    match &args.symbols {
        Some(symbols) => {
            for date in &dates {
                plan.extend(symbols.iter().map(|s| (*date, s.clone())));
            }
        }
        None => {
            // Exploration's screen as of each date, sampled across it.
            let cache = BarCache::today()?;
            let screen = Screen {
                min_price: strategy.universe.min_price,
                min_dollar_volume: strategy.universe.min_avg_dollar_volume,
                ..Screen::default()
            };
            let stocks = fetch_screened_pool(alpaca, &cache, &screen).await?;
            for date in &dates {
                let mut picked: Vec<(String, f64)> = screen
                    .select(&stocks, *date)
                    .into_iter()
                    .map(|(k, j)| {
                        let bar = &stocks[k].bars[j];
                        (stocks[k].symbol.clone(), bar.close * bar.volume as f64)
                    })
                    .filter(|(symbol, _)| !strategy.universe.excludes(symbol))
                    .collect();
                // Biggest to smallest, then evenly spaced picks for a mix.
                picked.sort_by(|a, b| b.1.total_cmp(&a.1));
                let n = args.per_date.min(picked.len());
                plan.extend((0..n).map(|i| (*date, picked[i * picked.len() / n.max(1)].0.clone())));
            }
        }
    }
    plan.retain(|item| !done.contains(item));
    Ok(plan)
}

/// Score the run's reports against what happened next.
async fn report(alpaca: &AlpacaClient, store: &Store) -> Result<()> {
    let reports = store.all_report_history()?;
    if reports.is_empty() {
        println!("no reports in this run yet");
        return Ok(());
    }
    let start = reports.iter().map(|r| r.generated_at).min().unwrap() - Duration::days(10);
    let mut bars: HashMap<String, Vec<Bar>> = HashMap::new();
    for report in &reports {
        if !bars.contains_key(&report.symbol) {
            bars.insert(
                report.symbol.clone(),
                alpaca.daily_history(&report.symbol, start).await?,
            );
        }
    }
    let spy = alpaca.daily_history(BENCHMARK, start).await?;
    let study = scorecard::study(&reports, &bars, &spy, Utc::now());

    let setups = reports
        .iter()
        .filter(|r| r.findings.entry_high.is_some())
        .count();
    println!(
        "\n{} reports, {setups} with a long setup (an entry range the executor could buy \
         within)",
        reports.len(),
    );
    print!("{}", scorecard::render_factors(&study));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_happen_after_the_close() {
        let t = moment("2026-07-15".parse().unwrap());
        assert_eq!(t.to_rfc3339(), "2026-07-15T21:30:00+00:00");
    }

    #[test]
    fn refuses_dates_before_the_cutoff() {
        let args: Vec<String> = ["--from", "2026-03-01"].map(String::from).to_vec();
        let err = run_cli(&args).unwrap_err().to_string();
        assert!(err.contains("knowledge cutoff"), "{err}");
    }
}
