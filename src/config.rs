//! User configuration: per-agent models and budgets, the executor's risk
//! limits, and the autonomous schedule. Lives at
//! `<config dir>/warren/config.toml` and is created with defaults on first
//! run. Risk limits are enforced in code; agents can read them but not change
//! them.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub schedule: Schedule,
    pub exploration: ExplorationConfig,
    pub research: ResearchConfig,
    pub executor: ExecutorConfig,
    pub jev: JevConfig,
    pub edgar: EdgarConfig,
}

/// SEC EDGAR, the source of sectors. The SEC requires a contact email in
/// each request; sector filters are off until one is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgarConfig {
    pub contact: String,
}

/// Shadow scoring with TypeSafe's Jev; see `agents::jev`. Needs `JEV_API_KEY`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JevConfig {
    pub enabled: bool,
    pub model: String,
    /// USD per million input tokens; TypeSafe bills input tokens only.
    pub input_price_per_million: f64,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "jev-latest".into(),
            input_price_per_million: 0.042,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// Start in autonomous mode. When false, Warren launches paused.
    pub autostart: bool,
    /// Minutes between executor cycles while the market is open.
    pub cycle_interval_minutes: u64,
    /// Hours between exploration runs.
    pub exploration_interval_hours: u64,
    /// Research reports older than this are refreshed before the executor
    /// relies on them.
    pub research_stale_after_hours: u64,
    /// Upper bound on research runs per cycle, to bound cost.
    pub max_research_per_cycle: usize,
    /// Hard cap on agent runs per calendar day across all agents.
    pub max_agent_runs_per_day: usize,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            autostart: false,
            cycle_interval_minutes: 120,
            exploration_interval_hours: 24,
            research_stale_after_hours: 24,
            max_research_per_cycle: 4,
            max_agent_runs_per_day: 40,
        }
    }
}

/// Model and turn budget, shared by every agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentModel {
    pub model: String,
    /// Model calls allowed per run, including tool-use turns.
    pub max_turns: usize,
    pub max_tokens: u64,
}

impl Default for AgentModel {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            max_turns: 24,
            max_tokens: 8192,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExplorationConfig {
    #[serde(flatten)]
    pub agent: AgentModel,
    /// Candidates nominated per run.
    pub max_candidates: usize,
    /// Screen out stocks below this price.
    pub min_price: f64,
    /// Screen out stocks trading less than this many dollars a day.
    pub min_avg_dollar_volume: f64,
}

impl Default for ExplorationConfig {
    fn default() -> Self {
        Self {
            agent: AgentModel::default(),
            max_candidates: 6,
            min_price: 5.0,
            min_avg_dollar_volume: 20_000_000.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResearchConfig {
    #[serde(flatten)]
    pub agent: AgentModel,
    /// Anthropic web searches allowed per stock researched; 0 turns search
    /// off. Never used in replays, where results could postdate the
    /// replayed moment.
    pub web_searches_per_stock: u32,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            agent: AgentModel::default(),
            web_searches_per_stock: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutorConfig {
    #[serde(flatten)]
    pub agent: AgentModel,
    /// Largest notional value of a single order, in dollars.
    pub max_order_notional: f64,
    /// Largest position in one symbol, as a percent of equity.
    pub max_position_percent: f64,
    /// Orders allowed per calendar day.
    pub max_orders_per_day: usize,
    /// Keep at least this percent of equity in cash.
    pub min_cash_percent: f64,
    /// Allow selling short. When false, sells are capped at shares held.
    pub allow_short: bool,
    /// Open positions allowed at once.
    pub max_positions: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            agent: AgentModel::default(),
            max_order_notional: 10_000.0,
            max_position_percent: 10.0,
            max_orders_per_day: 10,
            min_cash_percent: 10.0,
            allow_short: false,
            max_positions: 10,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("no config directory on this platform")?
            .join("warren")
            .join("config.toml"))
    }

    /// Load the config, writing the defaults out first if none exists.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            let config = Self::default();
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, toml::to_string_pretty(&config)?)
                .with_context(|| format!("writing {}", path.display()))?;
            return Ok(config);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.executor.max_order_notional, 10_000.0);
        assert_eq!(parsed.research.agent.model, "claude-opus-5-5");
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let parsed: Config = toml::from_str(
            "[executor]\nmax_order_notional = 2500.0\nmodel = \"claude-sonnet-5-5\"\n",
        )
        .unwrap();
        assert_eq!(parsed.executor.max_order_notional, 2500.0);
        assert_eq!(parsed.executor.agent.model, "claude-sonnet-5-5");
        assert_eq!(parsed.executor.max_orders_per_day, 10);
        assert_eq!(parsed.exploration.agent.model, "claude-opus-5-5");
    }
}
