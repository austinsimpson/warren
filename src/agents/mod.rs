//! The autonomous agents and what they share.
//!
//! - [`exploration`] scans the market for new candidates.
//! - [`research`] writes an in-depth report on one symbol.
//! - [`executor`] trades on the reports within the configured risk limits,
//!   journals every decision, and reviews past decisions to learn.
//!
//! Every run is traced live through [`AgentEvent`]s and logged to the
//! [`Store`] with its token usage. Runs execute on the tokio runtime.

pub mod autopilot;
pub mod executor;
pub mod exploration;
pub mod jev;
pub mod research;
pub mod scoring;
pub mod technicals;
pub mod tools;

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rig_agent::{
    AgentBuilder,
    agent::{
        AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
        HookContext, ModelTurnAction, ModelTurnFinished, OutcomeAction, OutcomeEvent,
    },
};
use rig_core::{
    catalog::Catalog,
    completion::{CacheRetention, Usage},
    providers::anthropic::Anthropic,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{AgentModel, Config};
use crate::market::alpaca::AlpacaClient;
use crate::store::{RunRecord, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Exploration,
    Research,
    Executor,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Exploration => "explore",
            Self::Research => "research",
            Self::Executor => "execute",
        })
    }
}

/// Live activity for the UI.
#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub at: DateTime<Utc>,
    /// `None` for scheduler messages.
    pub agent: Option<AgentKind>,
    pub subject: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug, Clone)]
pub enum EventKind {
    RunStarted,
    ToolCall {
        tool: String,
        args: String,
    },
    ToolResult {
        tool: String,
        ok: bool,
        output: String,
    },
    RunFinished {
        ok: bool,
        summary: String,
    },
    /// Scheduler status, e.g. "Market closed".
    Status(String),
}

/// Everything an agent run needs. Cheap to clone.
#[derive(Clone)]
pub struct AgentCtx {
    pub alpaca: Arc<AlpacaClient>,
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    pub events: UnboundedSender<AgentEvent>,
    /// Set to stop in-flight runs at their next step (the kill switch).
    pub stop: Arc<AtomicBool>,
    /// Shadow scorer, when enabled and configured.
    pub jev: Option<Arc<jev::JevScorer>>,
    /// Token usage of the run this context belongs to; see
    /// [`AgentCtx::recorded`].
    pub meter: Meter,
}

/// Tokens one run has spent, added as each model turn and Jev call finishes
/// so a run that fails partway still accounts for what it used.
#[derive(Clone, Default)]
pub struct Meter(Arc<Mutex<RunUsage>>);

#[derive(Debug, Clone, Copy, Default)]
pub struct RunUsage {
    pub model: Usage,
    pub jev_input_tokens: u64,
    pub jev_output_tokens: u64,
}

impl Meter {
    fn add_turn(&self, usage: Usage) {
        self.0.lock().unwrap().model += usage;
    }

    pub fn add_jev(&self, input_tokens: u64, output_tokens: u64) {
        let mut usage = self.0.lock().unwrap();
        usage.jev_input_tokens += input_tokens;
        usage.jev_output_tokens += output_tokens;
    }

    fn total(&self) -> RunUsage {
        *self.0.lock().unwrap()
    }
}

/// The list-price cost of `usage` on `model` in USD: the provider's figure
/// if it reported one, else priced from rig's model catalog. `None` for a
/// model the catalog doesn't price.
pub fn model_cost(model: &str, usage: &Usage) -> Option<f64> {
    if let Some(cost) = &usage.cost {
        return Some(cost.total);
    }
    let spec = Catalog::builtin().resolve(&format!("anthropic/{model}"))?;
    Some(spec.pricing?.cost(usage)?.total)
}

impl AgentCtx {
    pub fn emit(&self, agent: Option<AgentKind>, subject: Option<&str>, kind: EventKind) {
        let _ = self.events.send(AgentEvent {
            at: Utc::now(),
            agent,
            subject: subject.map(str::to_string),
            kind,
        });
    }

    pub fn status(&self, message: impl Into<String>) {
        self.emit(None, None, EventKind::Status(message.into()));
    }

    /// An agent builder for `model`, traced and stoppable.
    fn builder(
        &self,
        agent: AgentKind,
        subject: Option<&str>,
        model: &AgentModel,
        preamble: String,
    ) -> Result<AgentBuilder> {
        let completion = Anthropic::from_env()?.completion(model.model.clone());
        Ok(AgentBuilder::new(completion)
            .preamble(preamble)
            // Every turn resends the conversation so far, so turns after the
            // first read most of their input from the cache (a twentieth of
            // the price). Five-minute retention covers a run and the parallel
            // research runs of a cycle; cycles are too far apart for longer.
            .cache(CacheRetention::Short)
            .max_tokens(model.max_tokens)
            .default_max_turns(model.max_turns)
            .add_hook(TraceHook {
                ctx: self.clone(),
                agent,
                subject: subject.map(str::to_string),
            }))
    }

    /// The model settings `agent` runs with.
    fn agent_model(&self, agent: AgentKind) -> &AgentModel {
        match agent {
            AgentKind::Exploration => &self.config.exploration.agent,
            AgentKind::Research => &self.config.research.agent,
            AgentKind::Executor => &self.config.executor.agent,
        }
    }

    /// Run `work`, tracing its start and end and logging it to the store
    /// with its token usage and cost. `work` gets a context with a fresh
    /// [`Meter`], so agents built from it (and Jev calls made with it) are
    /// counted against this run, and returns its result and a one-line
    /// summary.
    async fn recorded<T>(
        &self,
        agent: AgentKind,
        subject: Option<&str>,
        work: impl AsyncFnOnce(&AgentCtx) -> Result<(T, String)>,
    ) -> Result<T> {
        let started_at = Utc::now();
        self.emit(Some(agent), subject, EventKind::RunStarted);
        let run_ctx = AgentCtx {
            meter: Meter::default(),
            ..self.clone()
        };
        let meter = run_ctx.meter.clone();
        let result = work(&run_ctx).await;

        let (ok, summary) = match &result {
            Ok((_, summary)) => (true, summary.clone()),
            Err(err) => (false, format!("{err:#}")),
        };
        let usage = meter.total();
        let model = &self.agent_model(agent).model;
        self.emit(
            Some(agent),
            subject,
            EventKind::RunFinished {
                ok,
                summary: summary.clone(),
            },
        );
        let record = RunRecord {
            agent,
            subject: subject.map(str::to_string),
            started_at,
            finished_at: Utc::now(),
            ok,
            summary,
            input_tokens: usage.model.input_tokens.unwrap_or_default(),
            output_tokens: usage.model.output_tokens.unwrap_or_default(),
            cached_input_tokens: usage.model.cached_input_tokens.unwrap_or_default(),
            cache_write_tokens: usage.model.cache_creation_input_tokens.unwrap_or_default(),
            model: Some(model.clone()),
            cost_usd: model_cost(model, &usage.model).map(|cost| {
                cost + usage.jev_input_tokens as f64 * self.config.jev.input_price_per_million
                    / 1_000_000.0
            }),
            jev_input_tokens: usage.jev_input_tokens,
            jev_output_tokens: usage.jev_output_tokens,
        };
        if let Err(err) = self.store.append_run(&record) {
            self.status(format!("Failed to log run: {err:#}"));
        }
        result.map(|(value, _)| value)
    }
}

/// Reports each tool call to the UI and stops the run when the kill switch is
/// set.
struct TraceHook {
    ctx: AgentCtx,
    agent: AgentKind,
    subject: Option<String>,
}

impl TraceHook {
    fn stopped(&self) -> bool {
        self.ctx.stop.load(Ordering::Relaxed)
    }
}

const STOP_REASON: &str = "Warren was paused";
/// Trace payloads are clipped to this many characters.
const TRACE_CHARS: usize = 400;

impl AgentHook for TraceHook {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        _: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if self.stopped() {
            CompletionCallAction::stop(STOP_REASON)
        } else {
            CompletionCallAction::Continue
        }
    }

    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        self.ctx.meter.add_turn(event.usage);
        ModelTurnAction::Continue
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let (Some(tool), Some(args)) = (event.tool_name(), event.tool_args()) else {
            return DispatchAction::Proceed;
        };
        if self.stopped() {
            return DispatchAction::stop(STOP_REASON);
        }
        self.ctx.emit(
            Some(self.agent),
            self.subject.as_deref(),
            EventKind::ToolCall {
                tool: tool.to_string(),
                args: clip(args),
            },
        );
        DispatchAction::Proceed
    }

    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let (Some(tool), Some(result)) = (event.tool_name(), event.tool_result()) {
            self.ctx.emit(
                Some(self.agent),
                self.subject.as_deref(),
                EventKind::ToolResult {
                    tool: tool.to_string(),
                    ok: result.is_success(),
                    output: clip(&result.output().render()),
                },
            );
        }
        OutcomeAction::Proceed
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(TRACE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

impl AgentCtx {
    pub fn new(
        alpaca: Arc<AlpacaClient>,
        store: Arc<Store>,
        config: Arc<Config>,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let jev = config
            .jev
            .enabled
            .then(|| jev::JevScorer::from_env(&config.jev.model));
        let ctx = Self {
            alpaca,
            store,
            config,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            jev: match jev {
                Some(Ok(scorer)) => Some(Arc::new(scorer)),
                _ => None,
            },
            meter: Meter::default(),
        };
        if ctx.config.jev.enabled && ctx.jev.is_none() {
            ctx.status("Jev shadow scoring disabled: set JEV_API_KEY");
        }
        (ctx, receiver)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A minimal report for store and UI tests.
    pub fn sample_report(symbol: &str, scoring_version: u32) -> research::ResearchReport {
        use scoring::{FactorScore, FactorScores, JudgedFactors, QuantFactors};
        let factor = || FactorScore {
            score: 3,
            reason: "test".into(),
        };
        let judged = JudgedFactors {
            catalyst: factor(),
            news_sentiment: factor(),
            setup_quality: factor(),
        };
        research::ResearchReport {
            symbol: symbol.into(),
            generated_at: Utc::now(),
            model: "test".into(),
            price_at_research: Some(10.0),
            scoring_version,
            rating: research::Rating::Hold,
            conviction: 0.5,
            risk_reward: None,
            factors: FactorScores {
                quant: QuantFactors::default(),
                judged: judged.clone(),
                risk_reward: None,
            },
            shadow: None,
            trend_health: None,
            findings: research::ResearchFindings {
                horizon_days: 10,
                thesis: String::new(),
                catalysts: vec![],
                risks: vec![],
                entry_low: None,
                entry_high: None,
                target_price: None,
                stop_loss: None,
                technical_summary: String::new(),
                news_summary: String::new(),
                factors: judged,
            },
        }
    }

    /// A live context over a throwaway store, printing events as they arrive.
    pub fn live_ctx() -> (AgentCtx, std::path::PathBuf) {
        dotenvy::dotenv().ok();
        let dir = std::env::temp_dir().join(format!("warren-live-{}", std::process::id()));
        let (ctx, mut events) = AgentCtx::new(
            Arc::new(AlpacaClient::from_env().unwrap()),
            Arc::new(Store::open(&dir).unwrap()),
            Arc::new(Config::default()),
        );
        crate::runtime::RUNTIME.spawn(async move {
            while let Some(event) = events.recv().await {
                println!("[{:?} {:?}] {:?}", event.agent, event.subject, event.kind);
            }
        });
        (ctx, dir)
    }
}
