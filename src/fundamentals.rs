//! Company fundamentals from SEC EDGAR's XBRL data: quarterly revenue,
//! gross profit, net income, and shares outstanding, and the growth metrics
//! built from them.
//!
//! Two sources feed one [`Series`] per company:
//! - company facts (`data.sec.gov/api/xbrl/companyfacts`), one company at a
//!   time with each figure's filing date, for live research;
//! - frames (`.../frames`), one figure for every company and quarter, for
//!   backtests, which treat a quarter as public 90 days after it ends.
//!
//! Companies rarely report a fourth quarter on its own, so it is derived
//! from the fiscal year less the three quarters within it.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
};

use anyhow::{Context as _, Result};
use chrono::{Datelike as _, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::market::edgar::{EdgarClient, Filing};

/// Revenue tags in order of preference: filers use different ones.
pub const REVENUE_TAGS: [&str; 4] = [
    "RevenueFromContractWithCustomerExcludingAssessedTax",
    "Revenues",
    "SalesRevenueNet",
    "RevenueFromContractWithCustomerIncludingAssessedTax",
];
/// Cost-of-revenue tags in order of preference.
pub const COST_TAGS: [&str; 2] = ["CostOfRevenue", "CostOfGoodsAndServicesSold"];
/// How long backtests wait before treating a quarter as public: 10-Qs are
/// due within 40–45 days and 10-Ks within 60–90.
pub const BACKTEST_LAG_DAYS: i64 = 90;

/// One figure for one period.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub start: Option<NaiveDate>,
    pub end: NaiveDate,
    pub value: f64,
    /// When it became public.
    pub available: NaiveDate,
}

impl Fact {
    fn days(&self) -> Option<i64> {
        Some((self.end - self.start?).num_days())
    }

    fn is_quarter(&self) -> bool {
        self.days().is_some_and(|d| (80..=100).contains(&d))
    }

    fn is_year(&self) -> bool {
        self.days().is_some_and(|d| (350..=380).contains(&d))
    }
}

/// A company's raw facts by metric.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Series {
    pub revenue: Vec<Fact>,
    pub gross_profit: Vec<Fact>,
    /// Cost of revenue, for gross profit where a filer doesn't report it.
    #[serde(default)]
    pub cost_of_revenue: Vec<Fact>,
    pub net_income: Vec<Fact>,
    /// Point-in-time share counts (no `start`).
    pub shares: Vec<Fact>,
}

/// Quarterly values known as of a date, keyed by quarter end.
fn quarterly(facts: &[Fact], as_of: NaiveDate) -> BTreeMap<NaiveDate, f64> {
    let known: Vec<&Fact> = facts.iter().filter(|f| f.available <= as_of).collect();
    // The first-reported value per period, so later restatements can't
    // reach back.
    let mut quarters: BTreeMap<NaiveDate, (NaiveDate, f64)> = BTreeMap::new();
    for f in known.iter().filter(|f| f.is_quarter()) {
        let entry = quarters.entry(f.end).or_insert((f.available, f.value));
        if f.available < entry.0 {
            *entry = (f.available, f.value);
        }
    }
    let mut out: BTreeMap<NaiveDate, f64> =
        quarters.into_iter().map(|(end, (_, v))| (end, v)).collect();
    // Fourth quarters: the year less the three quarters inside it.
    for year in known.iter().filter(|f| f.is_year()) {
        if out.contains_key(&year.end) {
            continue;
        }
        let start = year.start.expect("years have a start");
        let inside: Vec<f64> = out.range(start..year.end).map(|(_, v)| *v).collect();
        if inside.len() == 3 {
            out.insert(year.end, year.value - inside.iter().sum::<f64>());
        }
    }
    out
}

/// The quarter about a year before `end` in `series`, allowing for fiscal
/// calendars that drift a few days.
fn year_before(series: &BTreeMap<NaiveDate, f64>, end: NaiveDate) -> Option<f64> {
    let target = end - Duration::days(365);
    series
        .range(target - Duration::days(20)..=target + Duration::days(20))
        .next()
        .map(|(_, v)| *v)
}

/// Growth metrics as of a date, each `None` when the history is too thin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GrowthMetrics {
    /// The latest reported quarter's end.
    pub latest_quarter: Option<NaiveDate>,
    /// Latest quarter's revenue against the same quarter a year earlier.
    pub revenue_growth_percent: Option<f64>,
    /// That growth less the previous quarter's: positive is accelerating.
    pub acceleration_points: Option<f64>,
    pub ttm_revenue: Option<f64>,
    /// Trailing-twelve-month gross margin.
    pub gross_margin_percent: Option<f64>,
    /// Change in that margin from a year earlier, in points.
    pub gross_margin_change_points: Option<f64>,
    pub ttm_net_margin_percent: Option<f64>,
    /// Growth in shares outstanding over a year: dilution.
    pub share_growth_percent: Option<f64>,
    pub shares_outstanding: Option<f64>,
}

impl GrowthMetrics {
    /// Market cap and price-to-sales at `price`.
    pub fn valuation(&self, price: f64) -> (Option<f64>, Option<f64>) {
        let cap = self.shares_outstanding.map(|s| s * price);
        let ps = match (cap, self.ttm_revenue) {
            (Some(cap), Some(rev)) if rev > 0.0 => Some(cap / rev),
            _ => None,
        };
        (cap, ps)
    }
}

/// Sum of the four quarters ending at `end`, if all are present.
fn ttm(series: &BTreeMap<NaiveDate, f64>, end: NaiveDate) -> Option<f64> {
    let quarters: Vec<f64> = series
        .range(end - Duration::days(350)..=end)
        .map(|(_, v)| *v)
        .collect();
    (quarters.len() == 4).then(|| quarters.iter().sum())
}

impl Series {
    pub fn metrics(&self, as_of: NaiveDate) -> GrowthMetrics {
        let revenue = quarterly(&self.revenue, as_of);
        let gross = if self.gross_profit.is_empty() {
            // Revenue less cost of revenue, quarter by quarter.
            let cost = quarterly(&self.cost_of_revenue, as_of);
            revenue
                .iter()
                .filter_map(|(end, rev)| Some((*end, rev - cost.get(end)?)))
                .collect()
        } else {
            quarterly(&self.gross_profit, as_of)
        };
        let net = quarterly(&self.net_income, as_of);
        let mut m = GrowthMetrics::default();
        let Some((&latest, &rev_now)) = revenue.iter().next_back() else {
            return m;
        };
        m.latest_quarter = Some(latest);
        let growth = |end: NaiveDate, now: f64| {
            let then = year_before(&revenue, end)?;
            (then > 0.0).then(|| (now / then - 1.0) * 100.0)
        };
        m.revenue_growth_percent = growth(latest, rev_now);
        let previous = revenue.range(..latest).next_back();
        if let (Some(now), Some((&prev_end, &prev))) = (m.revenue_growth_percent, previous) {
            m.acceleration_points = growth(prev_end, prev).map(|g| now - g);
        }
        m.ttm_revenue = ttm(&revenue, latest);
        let margin = |end: NaiveDate| {
            let (g, r) = (ttm(&gross, end)?, ttm(&revenue, end)?);
            (r > 0.0).then(|| g / r * 100.0)
        };
        m.gross_margin_percent = margin(latest);
        if let Some(now) = m.gross_margin_percent {
            let year_ago = revenue
                .range(..=latest - Duration::days(345))
                .next_back()
                .map(|(end, _)| *end);
            m.gross_margin_change_points = year_ago.and_then(margin).map(|then| now - then);
        }
        m.ttm_net_margin_percent = match (ttm(&net, latest), m.ttm_revenue) {
            (Some(n), Some(r)) if r > 0.0 => Some(n / r * 100.0),
            _ => None,
        };
        let mut shares: Vec<&Fact> = self
            .shares
            .iter()
            .filter(|f| f.available <= as_of)
            .collect();
        shares.sort_by_key(|f| f.end);
        if let Some(now) = shares.last() {
            m.shares_outstanding = Some(now.value);
            let year_ago = shares
                .iter()
                .rev()
                .find(|f| (now.end - f.end).num_days() >= 330);
            m.share_growth_percent = year_ago
                .filter(|f| f.value > 0.0)
                .map(|f| (now.value / f.value - 1.0) * 100.0);
        }
        m
    }
}

/// Parse EDGAR company facts into a [`Series`].
pub fn from_company_facts(json: &serde_json::Value) -> Series {
    let facts = |taxonomy: &str, tag: &str, unit: &str, duration: bool| -> Vec<Fact> {
        json["facts"][taxonomy][tag]["units"][unit]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let date = |k: &str| r[k].as_str()?.parse::<NaiveDate>().ok();
                        Some(Fact {
                            start: if duration { Some(date("start")?) } else { None },
                            end: date("end")?,
                            value: r["val"].as_f64()?,
                            available: date("filed")?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    // The first revenue tag with any quarterly history wins, so one
    // company's series isn't stitched from tags with different scopes.
    let revenue = REVENUE_TAGS
        .iter()
        .map(|tag| facts("us-gaap", tag, "USD", true))
        .max_by_key(|f| f.iter().filter(|x| x.is_quarter()).map(|x| x.end).max())
        .unwrap_or_default();
    Series {
        revenue,
        gross_profit: facts("us-gaap", "GrossProfit", "USD", true),
        cost_of_revenue: COST_TAGS
            .iter()
            .map(|tag| facts("us-gaap", tag, "USD", true))
            .find(|f| !f.is_empty())
            .unwrap_or_default(),
        net_income: facts("us-gaap", "NetIncomeLoss", "USD", true),
        shares: {
            // The cover-page count, or, for filers with several share
            // classes that don't report one, the diluted quarterly average.
            let cover = facts("dei", "EntityCommonStockSharesOutstanding", "shares", false);
            if cover.is_empty() {
                facts(
                    "us-gaap",
                    "WeightedAverageNumberOfDilutedSharesOutstanding",
                    "shares",
                    true,
                )
                .into_iter()
                .filter(Fact::is_quarter)
                .collect()
            } else {
                cover
            }
        },
    }
}

/// Company facts are refetched after this long; new quarters arrive about
/// four times a year.
const FACTS_TTL_DAYS: i64 = 7;

#[derive(Serialize, Deserialize)]
struct CachedSeries {
    fetched_at: chrono::DateTime<chrono::Utc>,
    series: Series,
}

/// Each company's facts from EDGAR, fetched on demand and cached in
/// `<data dir>/warren/fundamentals/` (facts are megabytes; the parsed
/// series is small).
pub struct FundamentalsDirectory {
    edgar: EdgarClient,
    dir: std::path::PathBuf,
    tickers: tokio::sync::Mutex<Option<(chrono::DateTime<chrono::Utc>, HashMap<String, u64>)>>,
    /// Recently read filing texts by accession number, so paging through
    /// a long filing doesn't refetch it.
    texts: tokio::sync::Mutex<Vec<(String, std::sync::Arc<String>)>>,
}

/// Filing texts kept in memory.
const CACHED_TEXTS: usize = 8;

impl FundamentalsDirectory {
    pub fn open(edgar: EdgarClient, data_root: &std::path::Path) -> Result<Self> {
        let dir = data_root.join("fundamentals");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            edgar,
            dir,
            tickers: tokio::sync::Mutex::new(None),
            texts: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    async fn cik(&self, symbol: &str) -> Result<Option<u64>> {
        let now = chrono::Utc::now();
        let mut tickers = self.tickers.lock().await;
        if tickers
            .as_ref()
            .is_none_or(|(at, _)| now - *at > Duration::days(1))
        {
            *tickers = Some((now, self.edgar.tickers().await?));
        }
        Ok(tickers
            .as_ref()
            .and_then(|(_, map)| map.get(symbol).copied()))
    }

    /// The symbol's recent financial filings, newest first; `None` when the
    /// SEC has no filer under the ticker.
    pub async fn filings(&self, symbol: &str) -> Result<Option<Vec<Filing>>> {
        let Some(cik) = self.cik(symbol).await? else {
            return Ok(None);
        };
        Ok(Some(self.edgar.filings(cik).await?))
    }

    /// The readable text of one of the symbol's filings.
    pub async fn filing_text(
        &self,
        symbol: &str,
        filing: &Filing,
    ) -> Result<std::sync::Arc<String>> {
        if let Some((_, text)) = self
            .texts
            .lock()
            .await
            .iter()
            .find(|(accession, _)| accession == &filing.accession)
        {
            return Ok(text.clone());
        }
        let cik = self
            .cik(symbol)
            .await?
            .context("no SEC filer under this ticker")?;
        let text = std::sync::Arc::new(self.edgar.filing_text(cik, filing).await?);
        let mut texts = self.texts.lock().await;
        texts.push((filing.accession.clone(), text.clone()));
        let excess = texts.len().saturating_sub(CACHED_TEXTS);
        texts.drain(..excess);
        Ok(text)
    }

    /// The symbol's series, or `None` for companies without US-GAAP filings
    /// (most foreign issuers file under IFRS).
    pub async fn series(&self, symbol: &str) -> Result<Option<Series>> {
        let Some(cik) = self.cik(symbol).await? else {
            return Ok(None);
        };
        let path = self.dir.join(format!("{cik}.json"));
        if let Some(cached) = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CachedSeries>(&b).ok())
            .filter(|c| chrono::Utc::now() - c.fetched_at < Duration::days(FACTS_TTL_DAYS))
        {
            return Ok(Some(cached.series));
        }
        let series = from_company_facts(&self.edgar.company_facts(cik).await?);
        let cached = CachedSeries {
            fetched_at: chrono::Utc::now(),
            series,
        };
        fs::write(&path, serde_json::to_vec(&cached)?)?;
        Ok(Some(cached.series))
    }
}

/// A report's fundamentals: growth metrics as of the research moment and
/// the valuation at the research price.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub metrics: GrowthMetrics,
    pub market_cap: Option<f64>,
    pub price_to_sales: Option<f64>,
}

impl Snapshot {
    pub fn new(metrics: GrowthMetrics, price: Option<f64>) -> Self {
        let (market_cap, price_to_sales) = price.map_or((None, None), |p| metrics.valuation(p));
        Self {
            metrics,
            market_cap,
            price_to_sales,
        }
    }

    /// Lines for a prompt, skipping what's unknown.
    pub fn describe(&self) -> String {
        let m = &self.metrics;
        let pct = |v: Option<f64>, what: &str| v.map(|v| format!("{what} {v:+.1}%"));
        let money = |v: f64| {
            if v.abs() >= 1e9 {
                format!("${:.1}B", v / 1e9)
            } else {
                format!("${:.0}M", v / 1e6)
            }
        };
        [
            m.latest_quarter
                .map(|q| format!("latest quarter ended {q}")),
            pct(m.revenue_growth_percent, "revenue growth year over year"),
            m.acceleration_points.map(|a| {
                format!(
                    "growth {} by {:.1} points vs the prior quarter",
                    if a >= 0.0 {
                        "accelerated"
                    } else {
                        "decelerated"
                    },
                    a.abs()
                )
            }),
            m.ttm_revenue
                .map(|r| format!("trailing-12-month revenue {}", money(r))),
            m.gross_margin_percent
                .map(|g| format!("gross margin {g:.1}%")),
            m.gross_margin_change_points
                .map(|c| format!("gross margin change {c:+.1} points over a year")),
            m.ttm_net_margin_percent
                .map(|n| format!("net margin {n:.1}%")),
            pct(m.share_growth_percent, "share count change over a year"),
            self.market_cap.map(|c| format!("market cap {}", money(c))),
            self.price_to_sales.map(|p| format!("price/sales {p:.1}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ")
    }
}

/// Every company's series from EDGAR frames through `through`, keyed by
/// ticker (as Alpaca spells it). Each quarter counts as public
/// [`BACKTEST_LAG_DAYS`] after it ends. Frames are cached on disk; those for
/// the last two years are refreshed weekly since late filers keep adding to
/// them.
pub async fn series_from_frames(
    edgar: &EdgarClient,
    through: NaiveDate,
) -> Result<HashMap<String, Series>> {
    let cache = dirs::cache_dir()
        .context("no cache directory on this platform")?
        .join("warren")
        .join("frames");
    fs::create_dir_all(&cache)?;
    let lag = Duration::days(BACKTEST_LAG_DAYS);
    let mut periods: Vec<(String, NaiveDate)> = Vec::new(); // name, end
    for year in 2015..=through.year() {
        for q in 1..=4u32 {
            let end = NaiveDate::from_ymd_opt(year, q * 3, 1)
                .and_then(|d| d.checked_add_months(chrono::Months::new(1)))
                .map(|d| d - Duration::days(1))
                .expect("valid quarter end");
            if end <= through {
                periods.push((format!("CY{year}Q{q}"), end));
            }
        }
        let year_end = NaiveDate::from_ymd_opt(year, 12, 31).expect("valid date");
        if year_end <= through {
            periods.push((format!("CY{year}"), year_end));
        }
    }

    let fetch = async |taxonomy: &str, tag: &str, unit: &str, period: &str, end: NaiveDate| {
        let path = cache.join(format!("{taxonomy}_{tag}_{period}.json"));
        let fresh = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| {
                end < through - Duration::days(730) || age.as_secs() < 7 * 24 * 3600
            });
        if fresh
            && let Some(rows) = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
        {
            return Ok::<_, anyhow::Error>(rows);
        }
        let rows = edgar.frame(taxonomy, tag, unit, period).await?;
        fs::write(&path, serde_json::to_vec(&rows)?)?;
        Ok(rows)
    };
    let to_fact = |start: &Option<String>, end: &str, value: f64| -> Option<Fact> {
        let end: NaiveDate = end.parse().ok()?;
        Some(Fact {
            start: match start {
                Some(s) => Some(s.parse().ok()?),
                None => None,
            },
            end,
            value,
            available: end + lag,
        })
    };

    // Per company: facts by revenue tag, then the rest.
    let mut revenue: HashMap<u64, Vec<Vec<Fact>>> = HashMap::new();
    let mut by_cik: HashMap<u64, Series> = HashMap::new();
    let mut costs: HashMap<u64, Vec<Vec<Fact>>> = HashMap::new();
    let total = periods.len();
    for (i, (period, end)) in periods.iter().enumerate() {
        eprintln!("  fundamentals {}/{total}: {period}", i + 1);
        for (t, tag) in REVENUE_TAGS.iter().enumerate() {
            for (cik, start, e, v) in fetch("us-gaap", tag, "USD", period, *end).await? {
                if let Some(f) = to_fact(&start, &e, v) {
                    let tags = revenue.entry(cik).or_insert_with(|| vec![Vec::new(); 4]);
                    tags[t].push(f);
                }
            }
        }
        for (cik, start, e, v) in fetch("us-gaap", "GrossProfit", "USD", period, *end).await? {
            if let Some(f) = to_fact(&start, &e, v) {
                by_cik.entry(cik).or_default().gross_profit.push(f);
            }
        }
        for (t, tag) in COST_TAGS.iter().enumerate() {
            for (cik, start, e, v) in fetch("us-gaap", tag, "USD", period, *end).await? {
                if let Some(f) = to_fact(&start, &e, v) {
                    let tags = costs.entry(cik).or_insert_with(|| vec![Vec::new(); 2]);
                    tags[t].push(f);
                }
            }
        }
        if period.contains('Q') {
            let instant = format!("{period}I");
            for (cik, _, e, v) in fetch(
                "dei",
                "EntityCommonStockSharesOutstanding",
                "shares",
                &instant,
                *end,
            )
            .await?
            {
                if let Some(f) = to_fact(&None, &e, v) {
                    by_cik.entry(cik).or_default().shares.push(f);
                }
            }
        }
    }
    for (cik, tags) in revenue {
        // The tag with the most recent quarterly history, as in live data.
        let best = tags
            .into_iter()
            .max_by_key(|f| f.iter().filter(|x| x.is_quarter()).map(|x| x.end).max())
            .unwrap_or_default();
        by_cik.entry(cik).or_default().revenue = best;
    }

    for (cik, tags) in costs {
        // The first cost tag the company uses.
        let first = tags.into_iter().find(|f| !f.is_empty()).unwrap_or_default();
        by_cik.entry(cik).or_default().cost_of_revenue = first;
    }

    let tickers = edgar.tickers().await?;
    Ok(tickers
        .into_iter()
        .filter_map(|(symbol, cik)| Some((symbol, by_cik.get(&cik)?.clone())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fetches real company facts; run with
    /// `cargo test live_fundamentals -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_fundamentals() {
        let config = crate::config::Config::load_or_create().unwrap();
        let edgar = crate::market::edgar::EdgarClient::new(&config.edgar.contact).unwrap();
        crate::runtime::RUNTIME.block_on(async {
            let tickers = edgar.tickers().await.unwrap();
            for symbol in ["NVDA", "AAPL", "PLTR", "KO", "NU"] {
                let cik = tickers[symbol];
                let json = edgar.company_facts(cik).await.unwrap();
                let m = from_company_facts(&json).metrics(chrono::Utc::now().date_naive());
                println!("{symbol:5} {m:?}");
            }
        });
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn quarter(start: &str, end: &str, value: f64, filed: &str) -> Fact {
        Fact {
            start: Some(d(start)),
            end: d(end),
            value,
            available: d(filed),
        }
    }

    fn calendar_series() -> Series {
        // Two years of quarters; the fourth quarters only in annual figures.
        let mut revenue = vec![];
        for (year, base) in [(2024, 100.0), (2025, 150.0)] {
            let q = |n: u32| base + n as f64 * 10.0;
            revenue.push(quarter(
                &format!("{year}-01-01"),
                &format!("{year}-03-31"),
                q(0),
                &format!("{year}-05-01"),
            ));
            revenue.push(quarter(
                &format!("{year}-04-01"),
                &format!("{year}-06-30"),
                q(1),
                &format!("{year}-08-01"),
            ));
            revenue.push(quarter(
                &format!("{year}-07-01"),
                &format!("{year}-09-30"),
                q(2),
                &format!("{year}-11-01"),
            ));
            revenue.push(quarter(
                &format!("{year}-01-01"),
                &format!("{year}-12-31"),
                q(0) + q(1) + q(2) + q(3),
                &format!("{}-02-15", year + 1),
            ));
        }
        Series {
            revenue,
            ..Series::default()
        }
    }

    #[test]
    fn derives_fourth_quarters_and_growth() {
        let s = calendar_series();
        let m = s.metrics(d("2026-03-01"));
        assert_eq!(m.latest_quarter, Some(d("2025-12-31")));
        // Q4: 180 against 130.
        assert!((m.revenue_growth_percent.unwrap() - (180.0 / 130.0 - 1.0) * 100.0).abs() < 1e-9);
        assert_eq!(m.ttm_revenue, Some(150.0 + 160.0 + 170.0 + 180.0));
        // Q3 grew 170/120; Q4 grew 180/130, a little slower.
        let accel = (180.0 / 130.0 - 170.0 / 120.0) * 100.0;
        assert!((m.acceleration_points.unwrap() - accel).abs() < 1e-9);
    }

    #[test]
    fn ignores_figures_not_yet_public() {
        let s = calendar_series();
        // Before the 2025 annual report, Q3 2025 is the latest.
        let m = s.metrics(d("2026-01-15"));
        assert_eq!(m.latest_quarter, Some(d("2025-09-30")));
        assert!(s.metrics(d("2024-04-01")).latest_quarter.is_none());
    }

    #[test]
    fn gross_margin_from_cost_of_revenue() {
        let mut s = calendar_series();
        s.cost_of_revenue = s
            .revenue
            .iter()
            .map(|f| Fact {
                value: f.value * 0.4,
                ..*f
            })
            .collect();
        let m = s.metrics(d("2026-03-01"));
        assert!((m.gross_margin_percent.unwrap() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn valuation_from_shares_and_sales() {
        let m = GrowthMetrics {
            ttm_revenue: Some(1_000.0),
            shares_outstanding: Some(100.0),
            ..GrowthMetrics::default()
        };
        assert_eq!(m.valuation(50.0), (Some(5_000.0), Some(5.0)));
    }
}
