//! The autopilot in the UI: its controls, the live activity feed, and the
//! research card for the selected symbol.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, atomic::Ordering},
};

use chrono::Local;
use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*, px, rgb};

use crate::agents::book::Book;
use crate::agents::{
    AgentCtx, AgentEvent, AgentKind, EventKind,
    autopilot::{Autopilot, Control},
    exploration::CandidateList,
    research::{Rating, ResearchReport},
};
use crate::config::Config;
use crate::market::alpaca::AlpacaClient;
use crate::store::Store;
use crate::strategy::{Strategies, Strategy};
use crate::theme::*;
use crate::{Warren, runtime};

/// Activity lines kept in memory.
const MAX_EVENTS: usize = 300;

pub struct AgentsState {
    pub ctx: AgentCtx,
    pub control: Arc<Control>,
    /// The agent's book, refreshed with the portfolio.
    pub book: Option<Book>,
    events: VecDeque<AgentEvent>,
    pub reports: HashMap<String, ResearchReport>,
    pub candidates: Option<CandidateList>,
    /// Current-version research history per symbol, oldest first.
    history: HashMap<String, Vec<(chrono::DateTime<chrono::Utc>, f64, Rating)>>,
    runs_today: usize,
    /// Token and dollar totals from the run log.
    pub usage: crate::scorecard::Usage,
    /// Agents mid-run, with their subject.
    running: Vec<(AgentKind, Option<String>)>,
    status: SharedString,
}

impl AgentsState {
    pub fn store(&self) -> Arc<Store> {
        self.ctx.store.clone()
    }

    pub fn paused(&self) -> bool {
        self.ctx.stop.load(Ordering::Relaxed)
    }

    fn reload(&mut self) {
        if let Ok(reports) = self.ctx.store.reports() {
            self.reports = reports.into_iter().map(|r| (r.symbol.clone(), r)).collect();
        }
        if let Ok(history) = self.ctx.store.all_report_history() {
            self.history.clear();
            for report in history {
                self.history.entry(report.symbol).or_default().push((
                    report.generated_at,
                    report.conviction,
                    report.rating,
                ));
            }
        }
        if let Ok(candidates) = self.ctx.store.candidates() {
            self.candidates = candidates;
        }
        if let Ok(runs) = self.ctx.store.runs() {
            self.usage = crate::scorecard::usage(&runs, &self.ctx.config, chrono::Utc::now());
            self.runs_today = self.usage.today.runs;
        }
    }
}

/// Load the user's agents (moving pre-agent data into a "Default" one, or
/// creating it on first run), start each one's autopilot on the tokio
/// runtime, and pump their events into the view.
/// Where agents' settings live, and the app-wide config they build on, for
/// creating and editing agents.
pub struct AgentHome {
    pub strategies: Strategies,
    pub global: Config,
}

pub fn start(
    alpaca: Arc<AlpacaClient>,
    cx: &mut Context<Warren>,
) -> anyhow::Result<(Vec<AgentsState>, AgentHome)> {
    let global = Config::load_or_create()?;
    let data_root = Store::default_dir()?;
    let strategies = Strategies::new(&data_root);
    strategies.migrate_legacy(&data_root, &global)?;
    let mut list = strategies.list()?;
    if list.is_empty() {
        let default = Strategy::new("Default", 100_000.0);
        strategies.save(&default)?;
        list.push(default);
    }

    let mut events = None;
    let mut base: Option<AgentCtx> = None;
    let mut states = Vec::new();
    for strategy in list {
        let store = Arc::new(Store::open(strategies.dir(&strategy.id))?);
        let config = Arc::new(strategy.config(&global));
        let ctx = match &base {
            Some(base) => base.for_strategy(strategy, store, config),
            None => {
                let (ctx, receiver) = AgentCtx::new(alpaca.clone(), store, config, strategy);
                events = Some(receiver);
                base = Some(ctx.clone());
                ctx
            }
        };
        states.push(AgentsState::start(ctx));
    }
    let mut events = events.expect("at least one strategy");

    cx.spawn(async move |this, cx| {
        while let Some(event) = events.recv().await {
            if this
                .update(cx, |this, cx| this.on_agent_event(event, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();

    Ok((states, AgentHome { strategies, global }))
}

impl AgentsState {
    /// Spawn `ctx`'s autopilot and load its state.
    pub fn start(ctx: AgentCtx) -> Self {
        let autopilot = Autopilot::new(ctx.clone());
        let control = autopilot.control();
        runtime::RUNTIME.spawn(autopilot.run());
        let mut state = AgentsState {
            ctx,
            control,
            book: None,
            events: VecDeque::new(),
            reports: HashMap::new(),
            candidates: None,
            history: HashMap::new(),
            runs_today: 0,
            usage: Default::default(),
            running: Vec::new(),
            status: "Starting…".into(),
        };
        state.reload();
        // Seed the feed with recent history, newest first.
        if let Ok(runs) = state.ctx.store.recent_runs(30) {
            state.events = runs
                .into_iter()
                .map(|run| AgentEvent {
                    at: run.finished_at,
                    strategy: state.ctx.strategy.id.clone(),
                    agent: Some(run.agent),
                    subject: run.subject,
                    kind: EventKind::RunFinished {
                        ok: run.ok,
                        summary: run.summary,
                    },
                })
                .collect();
        }
        state
    }
}

impl Warren {
    /// The runtime of the agent the UI shows.
    pub fn agent(&self) -> Option<&AgentsState> {
        self.agents.get(self.selected_agent)
    }

    fn on_agent_event(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        let Some(agents) = self
            .agents
            .iter_mut()
            .find(|a| a.ctx.strategy.id == event.strategy)
        else {
            return;
        };
        match &event.kind {
            EventKind::RunStarted => {
                if let Some(agent) = event.agent {
                    agents.running.push((agent, event.subject.clone()));
                }
            }
            EventKind::RunFinished { .. } => {
                agents.running.retain(|(agent, subject)| {
                    Some(*agent) != event.agent || *subject != event.subject
                });
                agents.reload();
            }
            EventKind::Status(status) => agents.status = status.clone().into(),
            _ => {}
        }
        let executor_finished = event.agent == Some(AgentKind::Executor)
            && matches!(event.kind, EventKind::RunFinished { .. });
        if agents.events.len() == MAX_EVENTS {
            agents.events.pop_back();
        }
        agents.events.push_front(event);
        if executor_finished {
            self.refresh_portfolio(cx);
        }
        cx.notify();
    }

    fn toggle_autopilot(&mut self, cx: &mut Context<Self>) {
        if let Some(agents) = self.agent() {
            Autopilot::set_paused(&agents.ctx, &agents.control, !agents.paused());
            cx.notify();
        }
    }

    fn run_cycle_now(&mut self, cx: &mut Context<Self>) {
        if let Some(agents) = self.agent() {
            if agents.paused() {
                Autopilot::set_paused(&agents.ctx, &agents.control, false);
            }
            agents.control.run_now();
            cx.notify();
        }
    }

    /// Run/pause switch, run-now, and the daily run budget.
    pub fn render_autopilot_controls(&self, cx: &mut Context<Self>) -> Div {
        let Some(agents) = self.agent() else {
            return div().text_size(px(TEXT_XS)).text_color(rgb(RED)).child(
                self.agents_error
                    .clone()
                    .unwrap_or_else(|| "Autopilot unavailable".into()),
            );
        };
        let paused = agents.paused();
        let (dot, label_text) = if paused {
            (MUTED, "PAUSED")
        } else {
            (GREEN, "AUTO")
        };
        let budget = agents.ctx.config.schedule.max_agent_runs_per_day;

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .id("autopilot-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(tint(dot, 0.4))
                    .bg(tint(dot, 0.08))
                    .text_color(rgb(dot))
                    .text_size(px(TEXT_SM))
                    .font_weight(FontWeight::BOLD)
                    .cursor_pointer()
                    .hover(move |style| style.bg(tint(dot, 0.16)))
                    .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                    .child(label_text)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_autopilot(cx))),
            )
            .child(
                ghost_button("run-cycle", "Run cycle", false)
                    .on_click(cx.listener(|this, _, _, cx| this.run_cycle_now(cx))),
            )
            .child(label(format!(
                "runs {}/{budget} · ${:.2} today",
                agents.runs_today, agents.usage.today.cost_usd
            )))
    }

    /// What the agents are doing now, or the scheduler's last status.
    pub fn render_activity_status(&self) -> Option<Div> {
        self.agent().map(|agents| {
            let running: Vec<String> = agents
                .running
                .iter()
                .map(|(agent, subject)| match subject {
                    Some(subject) => format!("{agent} {subject}"),
                    None => agent.to_string(),
                })
                .collect();
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(TEXT_XS))
                .text_color(rgb(if running.is_empty() { MUTED } else { ACCENT }))
                .child(if running.is_empty() {
                    agents.status.to_string()
                } else {
                    format!("running: {}", running.join(", "))
                })
        })
    }

    /// Live feed of agent activity, newest first.
    pub fn render_activity(&self) -> impl IntoElement {
        let lines: Vec<Div> = self
            .agent()
            .into_iter()
            .flat_map(|agents| agents.events.iter().take(80))
            .map(activity_line)
            .collect();

        div()
            .id("activity")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .text_size(px(TEXT_SM))
            .children(lines)
    }

    /// Rating, conviction, levels, and thesis for the selected symbol.
    pub fn render_research_card(&self) -> Option<Div> {
        let agents = self.agent()?;
        let report = agents.reports.get(&self.selected)?;
        let f = &report.findings;
        let color = rating_color(report.rating);
        let level = |name: &'static str, value: Option<f64>| {
            value.map(|v| {
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(label(name))
                    .child(format!("{v:.2}"))
            })
        };
        let entry = f
            .entry_low
            .zip(f.entry_high)
            .map(|(lo, hi)| format!("{lo:.2}–{hi:.2}"));
        let age = chrono::Utc::now() - report.generated_at;
        let age = if age.num_hours() >= 24 {
            format!("{}d ago", age.num_days())
        } else if age.num_hours() >= 1 {
            format!("{}h ago", age.num_hours())
        } else {
            format!("{}m ago", age.num_minutes().max(0))
        };

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .min_w_0()
                .overflow_hidden()
                .px_2()
                .py_1p5()
                .rounded_sm()
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_x_3()
                        .gap_y_0p5()
                        .text_size(px(TEXT_SM))
                        .child(
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .bg(tint(color, 0.12))
                                .text_color(rgb(color))
                                .font_weight(FontWeight::BOLD)
                                .child(report.rating.to_string().replace('_', " ").to_uppercase()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .child(label("conv"))
                                .child(conviction_bar(report.conviction))
                                .child(format!("{:.2}", report.conviction)),
                        )
                        .children(entry.map(|e| {
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .child(label("entry"))
                                .child(e)
                        }))
                        .children(level("target", f.target_price))
                        .children(level("stop", f.stop_loss))
                        .child(label(format!("{}d · {age}", f.horizon_days))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_x_2()
                        .text_size(px(TEXT_XS))
                        .children(report.factors.scored().into_iter().map(|(name, factor)| {
                            // The r/r chip also shows the ratio behind its score,
                            // and judged factors show Jev's shadow score.
                            let ratio = (name == "r/r")
                                .then_some(report.risk_reward)
                                .flatten()
                                .map(|rr| label(format!("{rr:.1}:1")));
                            let shadow = report.shadow.as_ref().and_then(|s| match name {
                                "catalyst" => Some(&s.catalyst),
                                "news" => Some(&s.news_sentiment),
                                _ => None,
                            });
                            let shadow = shadow.map(|s| label(format!("jev {:.1}", s.score)));
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .child(label(name))
                                .child(
                                    div()
                                        .text_color(rgb(score_color(factor.score)))
                                        .child(factor.score.to_string()),
                                )
                                .children(ratio)
                                .children(shadow)
                        }))
                        // Shown, but not part of conviction.
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .child(label("context"))
                                .children(report.factors.context().into_iter().map(
                                    |(name, factor)| label(format!("{name} {}", factor.score)),
                                )),
                        )
                        .children(report.trend_health.map(|health| {
                            // Red when the executor would refuse to buy.
                            let vetoed = crate::agents::executor::check_trend_health(
                                Some(health),
                                &agents.ctx.config.executor,
                            )
                            .is_err();
                            let text = format!(
                                "52w high -{:.0}%{}",
                                health.percent_below_52w_high,
                                health
                                    .return_12_1_percent
                                    .map_or(String::new(), |r| format!(" · 12-1m {r:+.0}%")),
                            );
                            div()
                                .text_color(if vetoed { rgb(RED) } else { rgb(MUTED) })
                                .child(if vetoed {
                                    format!("{text} · no buys")
                                } else {
                                    text
                                })
                        })),
                )
                .children(self.render_conviction_history())
                .child(
                    div()
                        .w_full()
                        .font_family(SANS)
                        .text_size(px(TEXT_SM))
                        .text_color(rgb(TEXT))
                        .line_clamp(2)
                        .text_ellipsis()
                        .child(f.thesis.clone()),
                ),
        )
    }

    /// Conviction across every report on the selected symbol, with the change
    /// since the first and the buy threshold for reference.
    fn render_conviction_history(&self) -> Option<Div> {
        let agents = self.agent()?;
        let history = agents.history.get(&self.selected)?;
        let (first, last) = (history.first()?, history.last()?);
        let change = last.1 - first.1;
        let (change_text, change_color) = if history.len() < 2 {
            ("first report".to_string(), MUTED)
        } else if change.abs() < 0.005 {
            (format!("{:.2} → {:.2}, unchanged", first.1, last.1), MUTED)
        } else {
            (
                format!("{:.2} → {:.2} ({change:+.2})", first.1, last.1),
                if change > 0.0 { GREEN } else { RED },
            )
        };
        let since = first.0.with_timezone(&Local).format("%b %-d");
        let points = history
            .iter()
            .map(|(at, conviction, rating)| crate::chart::ConvictionPoint {
                at: *at,
                conviction: *conviction,
                color: rating_color(*rating),
            })
            .collect();
        let threshold = agents.ctx.config.executor.min_conviction_to_buy;

        Some(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_size(px(TEXT_XS))
                        .child(label("conviction history"))
                        .child(div().text_color(rgb(change_color)).child(change_text))
                        .child(div().flex_1())
                        .child(label(format!(
                            "{} report{} since {since}",
                            history.len(),
                            if history.len() == 1 { "" } else { "s" }
                        ))),
                )
                .child(
                    div()
                        .h(px(44.))
                        .w_full()
                        .rounded_sm()
                        .bg(tint(BORDER, 0.35))
                        .child(crate::chart::conviction_history(points, threshold)),
                ),
        )
    }

    /// Target and stop for the selected symbol, to mark on the chart.
    pub fn research_levels(&self) -> (Option<f64>, Option<f64>) {
        self.agent()
            .and_then(|agents| agents.reports.get(&self.selected))
            .map_or((None, None), |r| {
                (r.findings.target_price, r.findings.stop_loss)
            })
    }

    /// Candidate symbols from the latest exploration, by priority.
    pub fn candidate_symbols(&self) -> Vec<String> {
        self.agent()
            .and_then(|agents| agents.candidates.as_ref())
            .map(|list| list.candidates.iter().map(|c| c.symbol.clone()).collect())
            .unwrap_or_default()
    }
}

fn rating_color(rating: Rating) -> u32 {
    match rating {
        Rating::StrongBuy | Rating::Buy => GREEN,
        Rating::Hold => WARNING,
        Rating::Sell | Rating::StrongSell => RED,
    }
}

/// Bearish factor scores in red, neutral muted, bullish in green.
fn score_color(score: u8) -> u32 {
    match score {
        0..=2 => RED,
        3 => MUTED,
        _ => GREEN,
    }
}

fn conviction_bar(conviction: f64) -> Div {
    let width = 40.;
    div()
        .w(px(width))
        .h(px(4.))
        .rounded_full()
        .bg(rgb(SELECTED))
        .child(
            div()
                .h_full()
                .w(px(width * conviction.clamp(0.0, 1.0) as f32))
                .rounded_full()
                .bg(rgb(ACCENT)),
        )
}

fn agent_color(agent: Option<AgentKind>) -> u32 {
    match agent {
        Some(AgentKind::Exploration) => ACCENT,
        Some(AgentKind::Research) => 0xb48ef7,
        Some(AgentKind::Executor) => WARNING,
        None => MUTED,
    }
}

fn activity_line(event: &AgentEvent) -> Div {
    let (text, color): (String, u32) = match &event.kind {
        EventKind::RunStarted => ("started".into(), TEXT),
        EventKind::ToolCall { tool, args } => (format!("→ {tool} {args}"), MUTED),
        EventKind::ToolResult { tool, ok, output } => {
            (format!("← {tool} {output}"), if *ok { MUTED } else { RED })
        }
        EventKind::RunFinished { ok, summary } => (
            format!("{} {summary}", if *ok { "✓" } else { "✗" }),
            if *ok { TEXT } else { RED },
        ),
        EventKind::Status(status) => (status.clone(), MUTED),
    };
    let tag = match (event.agent, &event.subject) {
        (Some(agent), Some(subject)) => format!("{agent} {subject}"),
        (Some(agent), None) => agent.to_string(),
        (None, _) => "autopilot".into(),
    };

    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(
            div().flex_none().text_color(rgb(MUTED)).child(
                event
                    .at
                    .with_timezone(&Local)
                    .format("%H:%M:%S")
                    .to_string(),
            ),
        )
        .child(
            div()
                .flex_none()
                .w(px(110.))
                .truncate()
                .text_color(rgb(agent_color(event.agent)))
                .child(tag),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgb(color))
                .child(text),
        )
}
