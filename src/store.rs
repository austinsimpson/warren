//! Durable state shared by the agents and the UI, as JSON files under
//! `<data dir>/warren`: research reports, exploration candidates, the trade
//! journal, the executor's lessons, and a log of agent runs.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::agents::{
    AgentKind,
    exploration::CandidateList,
    jev::ShadowScores,
    research::{ResearchReport, SCORING_VERSION},
    scoring::FactorScores,
};
use crate::market::alpaca::{OrderSide, OrderType};

pub struct Store {
    dir: PathBuf,
    /// Serializes writes; reads go straight to disk.
    write_lock: Mutex<()>,
}

/// One order the executor placed, with the reasoning behind it, so later
/// runs can judge how the decision played out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub decided_at: DateTime<Utc>,
    pub symbol: String,
    pub side: OrderSide,
    pub qty: u32,
    pub order_type: OrderType,
    pub limit_price: Option<f64>,
    pub order_id: String,
    /// Last trade price when the decision was made.
    pub price_at_decision: f64,
    /// The benchmark (SPY) price at the same moment, for relative performance.
    pub benchmark_price_at_decision: Option<f64>,
    pub rationale: String,
    /// `generated_at` of the research report the decision relied on.
    pub research_generated_at: Option<DateTime<Utc>>,
    pub research_rating: Option<String>,
    pub research_conviction: Option<f64>,
    /// Factor scores behind that report, to learn which factors predict.
    #[serde(default)]
    pub research_factors: Option<FactorScores>,
    /// Jev's shadow scores on that report, to compare against the agent's.
    #[serde(default)]
    pub research_shadow: Option<ShadowScores>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    pub created_at: DateTime<Utc>,
    pub text: String,
}

/// One agent run, for the activity log and the daily run budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub agent: AgentKind,
    /// What the run was about, e.g. a symbol for research.
    pub subject: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub ok: bool,
    pub summary: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The most lessons kept; older ones are dropped as new ones arrive.
const MAX_LESSONS: usize = 30;

impl Store {
    pub fn open_default() -> Result<Self> {
        let dir = dirs::data_dir()
            .context("no data directory on this platform")?
            .join("warren");
        Self::open(dir)
    }

    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("research"))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            write_lock: Mutex::new(()),
        })
    }

    pub fn save_report(&self, report: &ResearchReport) -> Result<()> {
        self.write_json(&format!("research/{}.json", report.symbol), report)?;
        self.append_jsonl("research_history.jsonl", report)
    }

    /// The latest report for `symbol`. A report saved in an older format
    /// counts as missing, so the symbol gets researched again.
    pub fn report(&self, symbol: &str) -> Result<Option<ResearchReport>> {
        let path = self
            .dir
            .join(format!("research/{}.json", symbol.to_uppercase()));
        if !path.exists() {
            return Ok(None);
        }
        Ok(read_json_file(&path).ok())
    }

    /// The latest report for every researched symbol, newest first.
    pub fn reports(&self) -> Result<Vec<ResearchReport>> {
        let mut reports = Vec::new();
        for entry in fs::read_dir(self.dir.join("research"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                // Skip reports in an older format; see `report`.
                reports.extend(read_json_file(&path).ok());
            }
        }
        reports.sort_by(|a: &ResearchReport, b| b.generated_at.cmp(&a.generated_at));
        Ok(reports)
    }

    /// Current-version history for every symbol, oldest first.
    pub fn all_report_history(&self) -> Result<Vec<ResearchReport>> {
        Ok(self
            .history_lines()?
            .filter(|r| r.scoring_version == SCORING_VERSION)
            .collect())
    }

    fn history_lines(&self) -> Result<impl Iterator<Item = ResearchReport>> {
        let path = self.dir.join("research_history.jsonl");
        let text = if path.exists() {
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
        } else {
            String::new()
        };
        let reports: Vec<ResearchReport> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(reports.into_iter())
    }

    pub fn save_candidates(&self, candidates: &CandidateList) -> Result<()> {
        self.write_json("candidates.json", candidates)
    }

    pub fn candidates(&self) -> Result<Option<CandidateList>> {
        self.read_json("candidates.json")
    }

    pub fn append_journal(&self, entry: &JournalEntry) -> Result<()> {
        self.append_jsonl("journal.jsonl", entry)
    }

    /// Every journal entry, oldest first.
    pub fn journal(&self) -> Result<Vec<JournalEntry>> {
        self.read_jsonl("journal.jsonl")
    }

    pub fn lessons(&self) -> Result<Vec<Lesson>> {
        Ok(self.read_json("lessons.json")?.unwrap_or_default())
    }

    pub fn add_lesson(&self, text: String) -> Result<()> {
        let mut lessons = self.lessons()?;
        lessons.push(Lesson {
            created_at: Utc::now(),
            text,
        });
        let excess = lessons.len().saturating_sub(MAX_LESSONS);
        lessons.drain(..excess);
        self.write_json("lessons.json", &lessons)
    }

    pub fn append_run(&self, run: &RunRecord) -> Result<()> {
        self.append_jsonl("runs.jsonl", run)
    }

    /// The most recent `limit` runs, newest first.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let mut runs: Vec<RunRecord> = self.read_jsonl("runs.jsonl")?;
        runs.reverse();
        runs.truncate(limit);
        Ok(runs)
    }

    /// Runs started today, in local time.
    pub fn runs_today(&self) -> Result<usize> {
        let today = Local::now().date_naive();
        Ok(self
            .read_jsonl::<RunRecord>("runs.jsonl")?
            .iter()
            .filter(|run| run.started_at.with_timezone(&Local).date_naive() == today)
            .count())
    }

    /// The last successful run of `agent`, if any.
    pub fn last_successful_run(&self, agent: AgentKind) -> Result<Option<RunRecord>> {
        Ok(self
            .read_jsonl::<RunRecord>("runs.jsonl")?
            .into_iter()
            .rev()
            .find(|run| run.agent == agent && run.ok))
    }

    fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.dir.join(name);
        if !path.exists() {
            return Ok(None);
        }
        read_json_file(&path).map(Some)
    }

    /// Write via a temp file and rename, so readers never see a partial file.
    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let path = self.dir.join(name);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
    }

    fn append_jsonl<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let path = self.dir.join(name);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        file.write_all(&line)
            .with_context(|| format!("appending to {}", path.display()))
    }

    fn read_jsonl<T: DeserializeOwned>(&self, name: &str) -> Result<Vec<T>> {
        let path = self.dir.join(name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str(line)
                    .with_context(|| format!("{} line {}", path.display(), i + 1))
            })
            .collect()
    }
}

fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "warren-store-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn lessons_are_capped() {
        let (store, dir) = temp_store();
        for i in 0..MAX_LESSONS + 5 {
            store.add_lesson(format!("lesson {i}")).unwrap();
        }
        let lessons = store.lessons().unwrap();
        assert_eq!(lessons.len(), MAX_LESSONS);
        assert_eq!(lessons[0].text, "lesson 5");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runs_append_and_count() {
        let (store, dir) = temp_store();
        let run = |agent| RunRecord {
            agent,
            subject: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            ok: true,
            summary: "ok".into(),
            input_tokens: 1,
            output_tokens: 2,
        };
        store.append_run(&run(AgentKind::Research)).unwrap();
        store.append_run(&run(AgentKind::Executor)).unwrap();
        assert_eq!(store.runs_today().unwrap(), 2);
        assert_eq!(store.recent_runs(1).unwrap()[0].agent, AgentKind::Executor);
        assert!(
            store
                .last_successful_run(AgentKind::Exploration)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_keeps_current_version_and_skips_bad_lines() {
        let (store, dir) = temp_store();
        let live = crate::agents::test_support::sample_report("NU", SCORING_VERSION);
        let old = crate::agents::test_support::sample_report("NU", SCORING_VERSION - 1);
        store.append_jsonl("research_history.jsonl", &old).unwrap();
        store.append_jsonl("research_history.jsonl", &live).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.join("research_history.jsonl"))
            .unwrap()
            .write_all(b"{not json}\n")
            .unwrap();
        store.save_report(&live).unwrap();

        let history = store.all_report_history().unwrap();
        assert_eq!(history.len(), 2, "old version and bad line skipped");
        assert!(history.iter().all(|r| r.scoring_version == SCORING_VERSION));
        fs::remove_dir_all(dir).unwrap();
    }
}
