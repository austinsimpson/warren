//! User-created agents. Each is a strategy with its own settings, budget,
//! and data: it runs its own explore → research → execute loop and trades a
//! virtual book carved out of the shared paper account (see
//! `agents::book`).
//!
//! ("Agent" is what the UI calls these. In code they are strategies, since
//! `AgentKind` already names the explore, research, and execute stages.)
//!
//! Settings live in `<data dir>/warren/strategies/<id>/strategy.toml`, next
//! to that strategy's reports, journal, lessons, and run log.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{
    AgentModel, Config, ExecutorConfig, ExplorationConfig, ResearchConfig, Schedule,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Strategy {
    /// Stable, filesystem-safe; tags the strategy's orders.
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Start running at launch; otherwise it starts paused.
    #[serde(default)]
    pub autostart: bool,
    /// Dollars of the paper account this strategy may invest.
    pub budget: f64,
    #[serde(default)]
    pub risk: RiskTolerance,
    /// Individual limits that override the risk preset.
    #[serde(default)]
    pub overrides: LimitOverrides,
    #[serde(default)]
    pub universe: Universe,
    #[serde(default)]
    pub horizon: Horizon,
    #[serde(default)]
    pub schedule: StrategySchedule,
    #[serde(default)]
    pub models: Models,
    /// The user's own instructions, added to every stage's prompt. Hard
    /// limits still apply.
    #[serde(default)]
    pub instructions: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTolerance {
    Conservative,
    #[default]
    Moderate,
    Aggressive,
}

impl RiskTolerance {
    pub const ALL: [Self; 3] = [Self::Conservative, Self::Moderate, Self::Aggressive];

    pub fn label(self) -> &'static str {
        match self {
            Self::Conservative => "Conservative",
            Self::Moderate => "Moderate",
            Self::Aggressive => "Aggressive",
        }
    }

    /// The preset's limits, with sizes as percents of the budget.
    pub fn limits(self) -> Limits {
        match self {
            Self::Conservative => Limits {
                max_position_percent: 8.0,
                max_order_percent: 8.0,
                min_cash_percent: 20.0,
                max_positions: 8,
                max_orders_per_day: 6,
                min_conviction_to_buy: 0.65,
                max_percent_below_52w_high: 30.0,
                min_return_12_1_percent: -10.0,
            },
            Self::Moderate => Limits {
                max_position_percent: 10.0,
                max_order_percent: 10.0,
                min_cash_percent: 10.0,
                max_positions: 10,
                max_orders_per_day: 10,
                min_conviction_to_buy: 0.6,
                max_percent_below_52w_high: 45.0,
                min_return_12_1_percent: -20.0,
            },
            Self::Aggressive => Limits {
                max_position_percent: 20.0,
                max_order_percent: 20.0,
                min_cash_percent: 5.0,
                max_positions: 8,
                max_orders_per_day: 15,
                min_conviction_to_buy: 0.55,
                max_percent_below_52w_high: 60.0,
                min_return_12_1_percent: -35.0,
            },
        }
    }
}

/// Risk limits, with sizes relative to the strategy's budget.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub max_position_percent: f64,
    pub max_order_percent: f64,
    pub min_cash_percent: f64,
    pub max_positions: usize,
    pub max_orders_per_day: usize,
    pub min_conviction_to_buy: f64,
    pub max_percent_below_52w_high: f64,
    pub min_return_12_1_percent: f64,
}

/// Per-limit overrides of the preset; `None` keeps the preset's value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitOverrides {
    pub max_position_percent: Option<f64>,
    pub max_order_percent: Option<f64>,
    pub min_cash_percent: Option<f64>,
    pub max_positions: Option<usize>,
    pub max_orders_per_day: Option<usize>,
    pub min_conviction_to_buy: Option<f64>,
    pub max_percent_below_52w_high: Option<f64>,
    pub min_return_12_1_percent: Option<f64>,
}

impl LimitOverrides {
    /// Drop overrides that only repeat `preset`, so changing the preset
    /// later still moves them.
    pub fn without_preset_values(self, preset: Limits) -> Self {
        fn keep<T: PartialEq>(value: Option<T>, preset: T) -> Option<T> {
            value.filter(|v| *v != preset)
        }
        Self {
            max_position_percent: keep(self.max_position_percent, preset.max_position_percent),
            max_order_percent: keep(self.max_order_percent, preset.max_order_percent),
            min_cash_percent: keep(self.min_cash_percent, preset.min_cash_percent),
            max_positions: keep(self.max_positions, preset.max_positions),
            max_orders_per_day: keep(self.max_orders_per_day, preset.max_orders_per_day),
            min_conviction_to_buy: keep(self.min_conviction_to_buy, preset.min_conviction_to_buy),
            max_percent_below_52w_high: keep(
                self.max_percent_below_52w_high,
                preset.max_percent_below_52w_high,
            ),
            min_return_12_1_percent: keep(
                self.min_return_12_1_percent,
                preset.min_return_12_1_percent,
            ),
        }
    }

    fn apply(&self, preset: Limits) -> Limits {
        Limits {
            max_position_percent: self
                .max_position_percent
                .unwrap_or(preset.max_position_percent),
            max_order_percent: self.max_order_percent.unwrap_or(preset.max_order_percent),
            min_cash_percent: self.min_cash_percent.unwrap_or(preset.min_cash_percent),
            max_positions: self.max_positions.unwrap_or(preset.max_positions),
            max_orders_per_day: self.max_orders_per_day.unwrap_or(preset.max_orders_per_day),
            min_conviction_to_buy: self
                .min_conviction_to_buy
                .unwrap_or(preset.min_conviction_to_buy),
            max_percent_below_52w_high: self
                .max_percent_below_52w_high
                .unwrap_or(preset.max_percent_below_52w_high),
            min_return_12_1_percent: self
                .min_return_12_1_percent
                .unwrap_or(preset.min_return_12_1_percent),
        }
    }
}

/// Market sectors, roughly GICS's eleven. Assigned from SEC industry codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sector {
    Technology,
    Communication,
    ConsumerDiscretionary,
    ConsumerStaples,
    Energy,
    Financials,
    HealthCare,
    Industrials,
    Materials,
    RealEstate,
    Utilities,
}

impl Sector {
    pub const ALL: [Self; 11] = [
        Self::Technology,
        Self::Communication,
        Self::ConsumerDiscretionary,
        Self::ConsumerStaples,
        Self::Energy,
        Self::Financials,
        Self::HealthCare,
        Self::Industrials,
        Self::Materials,
        Self::RealEstate,
        Self::Utilities,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Technology => "Technology",
            Self::Communication => "Communication",
            Self::ConsumerDiscretionary => "Consumer discretionary",
            Self::ConsumerStaples => "Consumer staples",
            Self::Energy => "Energy",
            Self::Financials => "Financials",
            Self::HealthCare => "Health care",
            Self::Industrials => "Industrials",
            Self::Materials => "Materials",
            Self::RealEstate => "Real estate",
            Self::Utilities => "Utilities",
        }
    }
}

/// What the strategy may buy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Universe {
    /// Allowed sectors; empty allows every sector, including stocks whose
    /// sector is unknown.
    pub sectors: Vec<Sector>,
    pub min_price: f64,
    pub min_avg_dollar_volume: f64,
    /// Companies incorporated outside the US, including ADRs.
    pub allow_foreign: bool,
    pub excluded_symbols: Vec<String>,
}

impl Default for Universe {
    fn default() -> Self {
        Self {
            sectors: Vec::new(),
            min_price: 5.0,
            min_avg_dollar_volume: 20_000_000.0,
            allow_foreign: true,
            excluded_symbols: Vec::new(),
        }
    }
}

impl Universe {
    pub fn excludes(&self, symbol: &str) -> bool {
        self.excluded_symbols
            .iter()
            .any(|s| s.eq_ignore_ascii_case(symbol))
    }
}

/// How long positions are meant to be held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Horizon {
    /// Two to seven trading days.
    Short,
    /// One to four weeks.
    #[default]
    Swing,
    /// One to three months.
    Position,
}

impl Horizon {
    pub const ALL: [Self; 3] = [Self::Short, Self::Swing, Self::Position];

    pub fn label(self) -> &'static str {
        match self {
            Self::Short => "Short",
            Self::Swing => "Swing",
            Self::Position => "Position",
        }
    }

    /// For prompts: "holding periods of …".
    pub fn describe(self) -> &'static str {
        match self {
            Self::Short => "two to seven trading days",
            Self::Swing => "days to a few weeks",
            Self::Position => "one to three months",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategySchedule {
    pub cycle_interval_minutes: u64,
    pub exploration_interval_hours: u64,
    pub research_stale_after_hours: u64,
    pub max_research_per_cycle: usize,
    pub max_agent_runs_per_day: usize,
    pub max_candidates: usize,
}

impl Default for StrategySchedule {
    fn default() -> Self {
        let s = Schedule::default();
        Self {
            cycle_interval_minutes: s.cycle_interval_minutes,
            exploration_interval_hours: s.exploration_interval_hours,
            research_stale_after_hours: s.research_stale_after_hours,
            max_research_per_cycle: s.max_research_per_cycle,
            max_agent_runs_per_day: s.max_agent_runs_per_day,
            max_candidates: ExplorationConfig::default().max_candidates,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Models {
    pub exploration: AgentModel,
    pub research: AgentModel,
    pub executor: AgentModel,
}

impl Strategy {
    /// A new strategy with default settings.
    pub fn new(name: &str, budget: f64) -> Self {
        Self {
            id: slug(name),
            name: name.trim().to_string(),
            created_at: Utc::now(),
            autostart: false,
            budget,
            risk: RiskTolerance::default(),
            overrides: LimitOverrides::default(),
            universe: Universe::default(),
            horizon: Horizon::default(),
            schedule: StrategySchedule::default(),
            models: Models::default(),
            instructions: String::new(),
        }
    }

    /// The preset's limits with any overrides applied.
    pub fn limits(&self) -> Limits {
        self.overrides.apply(self.risk.limits())
    }

    /// Prefix of the client order IDs of this strategy's orders.
    pub fn order_prefix(&self) -> String {
        format!("w-{}-", self.id)
    }

    /// A fresh client order ID for this strategy.
    pub fn new_order_id(&self) -> String {
        let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        format!("{}{nanos:x}", self.order_prefix())
    }

    /// The per-stage configuration the agents read, from these settings and
    /// the app-wide ones in `global`. Shorting is never allowed: the paper
    /// account nets every strategy's shares in one position.
    pub fn config(&self, global: &Config) -> Config {
        let limits = self.limits();
        Config {
            schedule: Schedule {
                autostart: self.autostart,
                cycle_interval_minutes: self.schedule.cycle_interval_minutes,
                exploration_interval_hours: self.schedule.exploration_interval_hours,
                research_stale_after_hours: self.schedule.research_stale_after_hours,
                max_research_per_cycle: self.schedule.max_research_per_cycle,
                max_agent_runs_per_day: self.schedule.max_agent_runs_per_day,
            },
            exploration: ExplorationConfig {
                agent: self.models.exploration.clone(),
                max_candidates: self.schedule.max_candidates,
                min_price: self.universe.min_price,
                min_avg_dollar_volume: self.universe.min_avg_dollar_volume,
            },
            research: ResearchConfig {
                agent: self.models.research.clone(),
            },
            executor: ExecutorConfig {
                agent: self.models.executor.clone(),
                max_order_notional: self.budget * limits.max_order_percent / 100.0,
                max_position_percent: limits.max_position_percent,
                max_orders_per_day: limits.max_orders_per_day,
                min_cash_percent: limits.min_cash_percent,
                allow_short: false,
                min_conviction_to_buy: limits.min_conviction_to_buy,
                max_percent_below_52w_high: limits.max_percent_below_52w_high,
                min_return_12_1_percent: limits.min_return_12_1_percent,
                max_positions: limits.max_positions,
            },
            ..global.clone()
        }
    }

    /// The prompt section every stage gets: horizon, universe, and the
    /// user's instructions.
    pub fn prompt_context(&self) -> String {
        let sectors = if self.universe.sectors.is_empty() {
            "any".to_string()
        } else {
            self.universe
                .sectors
                .iter()
                .map(|s| s.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = format!(
            "\n\nThis agent: \"{name}\" ({risk} risk, holding periods of {horizon}).\n\
             - Sectors: {sectors}.{foreign}{excluded}\n\
             - Universe: price at least ${min_price:.2}, 20-day average dollar volume at \
             least ${min_dv:.0}.",
            name = self.name,
            risk = self.risk.label().to_lowercase(),
            horizon = self.horizon.describe(),
            foreign = if self.universe.allow_foreign {
                ""
            } else {
                "\n- US companies only: no foreign issuers or ADRs."
            },
            excluded = if self.universe.excluded_symbols.is_empty() {
                String::new()
            } else {
                format!(
                    "\n- Never consider: {}.",
                    self.universe.excluded_symbols.join(", ")
                )
            },
            min_price = self.universe.min_price,
            min_dv = self.universe.min_avg_dollar_volume,
        );
        let instructions = self.instructions.trim();
        if !instructions.is_empty() {
            text += &format!(
                "\n\nThe user's instructions for this agent (hard limits still apply):\n\
                 {instructions}"
            );
        }
        text
    }

    /// Check settings a user could get wrong.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.id.is_empty() {
            bail!("give the agent a name");
        }
        if self.budget <= 0.0 {
            bail!("the budget must be positive");
        }
        let l = self.limits();
        for (name, value) in [
            ("max position", l.max_position_percent),
            ("max order", l.max_order_percent),
            ("cash reserve", l.min_cash_percent),
        ] {
            if !(0.0..=100.0).contains(&value) {
                bail!("{name} must be between 0% and 100%");
            }
        }
        if !(0.0..=1.0).contains(&l.min_conviction_to_buy) {
            bail!("minimum conviction must be between 0 and 1");
        }
        Ok(())
    }
}

/// A filesystem- and order-ID-safe ID from a name.
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').chars().take(40).collect()
}

/// Where strategies live: one directory each under `root/strategies`.
pub struct Strategies {
    root: PathBuf,
}

impl Strategies {
    pub fn new(data_root: &Path) -> Self {
        Self {
            root: data_root.join("strategies"),
        }
    }

    /// The directory holding `id`'s settings and data.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir(id).join("strategy.toml")
    }

    /// Every strategy, oldest first. Unreadable ones are skipped with a
    /// message.
    pub fn list(&self) -> Result<Vec<Strategy>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path().join("strategy.toml");
            if !path.exists() {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(toml::from_str::<Strategy>(&text)?))
            {
                Ok(strategy) => out.push(strategy),
                Err(err) => eprintln!("skipping {}: {err:#}", path.display()),
            }
        }
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    /// Create or update `strategy`.
    pub fn save(&self, strategy: &Strategy) -> Result<()> {
        strategy.validate()?;
        let dir = self.dir(&strategy.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.file(&strategy.id);
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(strategy)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Move a pre-strategy data directory's files into a "Default"
    /// strategy built from the old app-wide settings. Does nothing once any
    /// strategy exists.
    pub fn migrate_legacy(&self, data_root: &Path, global: &Config) -> Result<()> {
        if !self.list()?.is_empty() {
            return Ok(());
        }
        let legacy = [
            "research",
            "research_history.jsonl",
            "candidates.json",
            "journal.jsonl",
            "lessons.json",
            "runs.jsonl",
        ];
        if !legacy.iter().any(|name| data_root.join(name).exists()) {
            return Ok(());
        }
        let mut strategy = Strategy::new("Default", 100_000.0);
        let old = &global.executor;
        strategy.autostart = global.schedule.autostart;
        strategy.overrides = LimitOverrides {
            max_position_percent: Some(old.max_position_percent),
            max_order_percent: Some(old.max_order_notional / strategy.budget * 100.0),
            min_cash_percent: Some(old.min_cash_percent),
            max_orders_per_day: Some(old.max_orders_per_day),
            min_conviction_to_buy: Some(old.min_conviction_to_buy),
            max_percent_below_52w_high: Some(old.max_percent_below_52w_high),
            min_return_12_1_percent: Some(old.min_return_12_1_percent),
            ..LimitOverrides::default()
        }
        .without_preset_values(strategy.risk.limits());
        strategy.universe.min_price = global.exploration.min_price;
        strategy.universe.min_avg_dollar_volume = global.exploration.min_avg_dollar_volume;
        strategy.models = Models {
            exploration: global.exploration.agent.clone(),
            research: global.research.agent.clone(),
            executor: global.executor.agent.clone(),
        };
        self.save(&strategy)?;
        let dir = self.dir(&strategy.id);
        for name in legacy {
            let from = data_root.join(name);
            if from.exists() {
                fs::rename(&from, dir.join(name))
                    .with_context(|| format!("moving {}", from.display()))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_safe() {
        assert_eq!(
            slug("  Mid-cap Software / Earnings! "),
            "mid-cap-software-earnings"
        );
        assert_eq!(slug("ÜBER"), "ber");
    }

    #[test]
    fn overrides_win_over_the_preset() {
        let mut s = Strategy::new("Test", 50_000.0);
        s.risk = RiskTolerance::Conservative;
        s.overrides.max_positions = Some(3);
        let l = s.limits();
        assert_eq!(l.max_positions, 3);
        assert_eq!(l.min_cash_percent, 20.0);
        let config = s.config(&Config::default());
        assert_eq!(config.executor.max_order_notional, 4_000.0);
        assert!(!config.executor.allow_short);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut s = Strategy::new("Energy swing", 25_000.0);
        s.universe.sectors = vec![Sector::Energy, Sector::Materials];
        s.instructions = "Favor names with insider buying.".into();
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(toml::from_str::<Strategy>(&text).unwrap(), s);
    }

    #[test]
    fn migrates_legacy_data_once() {
        let root = std::env::temp_dir().join(format!("warren-migrate-{}", std::process::id()));
        fs::create_dir_all(root.join("research")).unwrap();
        fs::write(root.join("journal.jsonl"), "").unwrap();
        let strategies = Strategies::new(&root);
        strategies
            .migrate_legacy(&root, &Config::default())
            .unwrap();
        let list = strategies.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "default");
        assert!(strategies.dir("default").join("journal.jsonl").exists());
        assert!(!root.join("journal.jsonl").exists());
        // The old settings carry over, without pinning preset values.
        assert_eq!(list[0].limits().max_order_percent, 10.0);
        assert_eq!(list[0].overrides, LimitOverrides::default());
        strategies
            .migrate_legacy(&root, &Config::default())
            .unwrap();
        assert_eq!(strategies.list().unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_context_includes_instructions() {
        let mut s = Strategy::new("Test", 1_000.0);
        s.universe.allow_foreign = false;
        s.instructions = "Only buy after earnings beats.".into();
        let text = s.prompt_context();
        assert!(text.contains("US companies only"));
        assert!(text.contains("Only buy after earnings beats."));
    }
}
