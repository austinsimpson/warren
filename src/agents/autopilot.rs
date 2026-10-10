//! The autonomous loop. While running and the market is open, each cycle:
//! explores for new candidates when exploration is due, refreshes stale
//! research on holdings and top candidates, then runs the executor. Every
//! step respects the daily run budget and the pause switch.

use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use anyhow::Result;
use chrono::{Local, Utc};
use tokio::sync::Notify;

use super::{AgentCtx, AgentKind, book::Book, executor, exploration, research, sweep};
use crate::config::Config;
use crate::strategy::Strategy;

/// Shared between the loop and the UI.
pub struct Control {
    wake: Notify,
    run_now: std::sync::atomic::AtomicBool,
    /// Edited settings for the loop to adopt before its next step.
    pending: std::sync::Mutex<Option<(Arc<Strategy>, Arc<Config>)>>,
}

impl Control {
    /// Hand the loop new settings. Runs already in flight finish with the
    /// old ones.
    pub fn update(&self, strategy: Arc<Strategy>, config: Arc<Config>) {
        *self.pending.lock().unwrap() = Some((strategy, config));
        self.wake.notify_one();
    }

    /// Start a cycle immediately, even if the market is closed.
    pub fn run_now(&self) {
        self.run_now.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    fn wake(&self) {
        self.wake.notify_one();
    }
}

/// While closed, check the clock at least this often.
const CLOSED_POLL: Duration = Duration::from_secs(30 * 60);

pub struct Autopilot {
    ctx: AgentCtx,
    control: Arc<Control>,
}

impl Autopilot {
    pub fn new(ctx: AgentCtx) -> Self {
        ctx.stop
            .store(!ctx.config.schedule.autostart, Ordering::Relaxed);
        Self {
            ctx,
            control: Arc::new(Control {
                wake: Notify::new(),
                run_now: false.into(),
                pending: Default::default(),
            }),
        }
    }

    pub fn control(&self) -> Arc<Control> {
        self.control.clone()
    }

    pub fn is_paused(&self) -> bool {
        self.ctx.stop.load(Ordering::Relaxed)
    }

    /// Pause stops in-flight runs at their next step and holds the loop.
    pub fn set_paused(ctx: &AgentCtx, control: &Control, paused: bool) {
        ctx.stop.store(paused, Ordering::Relaxed);
        ctx.status(if paused { "Paused" } else { "Running" });
        control.wake();
    }

    /// Run forever on the tokio runtime.
    pub async fn run(mut self) {
        let mut next_cycle = tokio::time::Instant::now();
        loop {
            if let Some((strategy, config)) = self.control.pending.lock().unwrap().take() {
                self.ctx.strategy = strategy;
                self.ctx.config = config;
            }
            let ctx = &self.ctx;
            if self.is_paused() {
                ctx.status("Paused");
                self.control.wake.notified().await;
                continue;
            }
            // A wake that is neither a run-now request nor due is spurious
            // (e.g. a resume): keep waiting for the scheduled cycle.
            let forced = self.control.run_now.swap(false, Ordering::Relaxed);
            if !forced && tokio::time::Instant::now() < next_cycle {
                tokio::select! {
                    _ = tokio::time::sleep_until(next_cycle) => {}
                    _ = self.control.wake.notified() => {}
                }
                continue;
            }

            let wait = match self.cycle(forced).await {
                Ok(wait) => wait,
                Err(err) => {
                    ctx.status(format!("Cycle failed: {err:#}"));
                    Duration::from_secs(5 * 60)
                }
            };
            next_cycle = tokio::time::Instant::now() + wait;
            if !self.is_paused() {
                let resume = Local::now() + chrono::Duration::from_std(wait).unwrap_or_default();
                ctx.status(format!("Idle until {}", resume.format("%-I:%M %p")));
            }
        }
    }

    /// One pass of explore → research → execute. Returns how long to wait
    /// before the next.
    async fn cycle(&self, forced: bool) -> Result<Duration> {
        let ctx = &self.ctx;
        let schedule = &ctx.config.schedule;
        let clock = ctx.alpaca.clock().await?;
        if !clock.is_open && !forced {
            let until_open = (clock.next_open - Utc::now()).to_std().unwrap_or_default();
            ctx.status(format!(
                "Market closed; opens {}",
                clock.next_open.with_timezone(&Local).format("%a %-I:%M %p")
            ));
            return Ok(until_open.min(CLOSED_POLL).max(Duration::from_secs(60)));
        }
        ctx.status(if clock.is_open {
            "Cycle started"
        } else {
            "Cycle started (market closed; orders queue for the open)"
        });

        if self.exploration_due()? && self.may_run()? {
            let _ = exploration::run(ctx).await;
        }

        for symbol in self.research_queue().await? {
            if !self.may_run()? {
                break;
            }
            let _ = research::run(ctx, &symbol).await;
        }

        if self.may_run()? {
            let _ = executor::run(ctx).await;
        }
        if clock.is_open
            && let Err(err) = sweep::park_idle_cash(ctx).await
        {
            ctx.status(format!("Cash sweep failed: {err:#}"));
        }
        Ok(Duration::from_secs(schedule.cycle_interval_minutes * 60))
    }

    /// Whether another agent run is allowed: not paused and within budget.
    fn may_run(&self) -> Result<bool> {
        if self.is_paused() {
            return Ok(false);
        }
        let max = self.ctx.config.schedule.max_agent_runs_per_day;
        if self.ctx.store.runs_today()? >= max {
            self.ctx
                .status(format!("Daily budget of {max} agent runs reached"));
            return Ok(false);
        }
        Ok(true)
    }

    fn exploration_due(&self) -> Result<bool> {
        let interval =
            chrono::Duration::hours(self.ctx.config.schedule.exploration_interval_hours as i64);
        Ok(
            match self.ctx.store.last_successful_run(AgentKind::Exploration)? {
                Some(run) => Utc::now() - run.finished_at >= interval,
                None => true,
            },
        )
    }

    /// Holdings first, then candidates by priority, keeping only those with
    /// no report or a stale one, up to the per-cycle cap.
    async fn research_queue(&self) -> Result<Vec<String>> {
        let ctx = &self.ctx;
        let schedule = &ctx.config.schedule;
        let stale_after = chrono::Duration::hours(schedule.research_stale_after_hours as i64);

        let holdings: Vec<String> = Book::load(&ctx.alpaca, &ctx.strategy)
            .await?
            .positions
            .into_iter()
            .map(|h| h.symbol)
            .collect();
        let candidates = ctx
            .store
            .candidates()?
            .map(|list| list.candidates)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.symbol);

        let mut seen = HashSet::new();
        let mut queue = Vec::new();
        for symbol in holdings.into_iter().chain(candidates) {
            if !seen.insert(symbol.clone()) {
                continue;
            }
            let fresh = ctx
                .store
                .report(&symbol)?
                .is_some_and(|r| Utc::now() - r.generated_at < stale_after);
            if !fresh {
                queue.push(symbol);
            }
            if queue.len() == schedule.max_research_per_cycle {
                break;
            }
        }
        Ok(queue)
    }
}
