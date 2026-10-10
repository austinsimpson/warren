//! A minimal SEC EDGAR client: ticker → CIK, and each company's SEC
//! industry (SIC) code and state or country of incorporation.
//!
//! Docs: <https://www.sec.gov/search-filings/edgar-application-programming-interfaces>
//!
//! The SEC asks for a User-Agent naming the requester with a contact
//! address and for no more than 10 requests a second; this client spaces
//! its requests accordingly. Must run on the tokio runtime.

use std::{collections::HashMap, time::Duration};

use anyhow::{Context as _, Result, bail};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use tokio::{sync::Mutex, time::Instant};

const TICKERS_URL: &str = "https://www.sec.gov/files/company_tickers.json";
const SUBMISSIONS_URL: &str = "https://data.sec.gov/submissions";
/// Comfortably under the SEC's 10 requests a second.
const MIN_INTERVAL: Duration = Duration::from_millis(125);

pub struct EdgarClient {
    http: reqwest::Client,
    user_agent: String,
    next_request: Mutex<Instant>,
}

/// What Warren needs from a company's EDGAR record.
#[derive(Debug, Clone, PartialEq)]
pub struct CompanyInfo {
    pub name: String,
    pub sic: Option<u32>,
    pub sic_description: String,
    /// EDGAR's state or country code, e.g. `DE` or `E9` (Cayman Islands).
    pub incorporated_in: String,
}

impl CompanyInfo {
    /// Incorporated outside the US. EDGAR's US state codes are two letters;
    /// every country and Canadian province code contains a digit.
    pub fn is_foreign(&self) -> bool {
        self.incorporated_in.chars().any(|c| c.is_ascii_digit())
    }
}

impl EdgarClient {
    /// `contact` is the email the SEC asks requesters to give.
    pub fn new(contact: &str) -> Result<Self> {
        if !contact.contains('@') {
            bail!("EDGAR needs a contact email (edgar.contact in config.toml)");
        }
        Ok(Self {
            http: reqwest::Client::new(),
            user_agent: format!("Warren research tool {contact}"),
            next_request: Mutex::new(Instant::now()),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        {
            let mut next = self.next_request.lock().await;
            tokio::time::sleep_until(*next).await;
            *next = Instant::now() + MIN_INTERVAL;
        }
        let response = self
            .http
            .get(url)
            .header(USER_AGENT, &self.user_agent)
            .send()
            .await
            .with_context(|| format!("EDGAR request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("EDGAR {url} returned {status}");
        }
        response
            .json()
            .await
            .with_context(|| format!("decoding EDGAR response from {url}"))
    }

    /// Every ticker the SEC maps to a company, as Alpaca spells them
    /// (`BRK.B`, not `BRK-B`), to its CIK.
    pub async fn tickers(&self) -> Result<HashMap<String, u64>> {
        #[derive(Deserialize)]
        struct Row {
            cik_str: u64,
            ticker: String,
        }
        let rows: HashMap<String, Row> = self.get_json(TICKERS_URL).await?;
        Ok(rows
            .into_values()
            .map(|row| (row.ticker.replace('-', "."), row.cik_str))
            .collect())
    }

    /// Every XBRL fact the company has filed: large (megabytes for big
    /// filers), so callers cache it.
    #[allow(dead_code)] // for the growth style, in progress
    pub async fn company_facts(&self, cik: u64) -> Result<serde_json::Value> {
        self.get_json(&format!(
            "https://data.sec.gov/api/xbrl/companyfacts/CIK{cik:010}.json"
        ))
        .await
    }

    /// One figure for every company in one period, e.g. tag `Revenues`,
    /// unit `USD`, period `CY2024Q3` (`CY2024Q3I` for point-in-time
    /// figures, `CY2024` for years). Rows are (CIK, start, end, value).
    pub async fn frame(
        &self,
        taxonomy: &str,
        tag: &str,
        unit: &str,
        period: &str,
    ) -> Result<Vec<(u64, Option<String>, String, f64)>> {
        let url =
            format!("https://data.sec.gov/api/xbrl/frames/{taxonomy}/{tag}/{unit}/{period}.json");
        let json: serde_json::Value = match self.get_json(&url).await {
            Ok(json) => json,
            // Periods nobody reported under this tag are 404s.
            Err(err) if err.to_string().contains("404") => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(json["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        Some((
                            r["cik"].as_u64()?,
                            r["start"].as_str().map(String::from),
                            r["end"].as_str()?.to_string(),
                            r["val"].as_f64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn company(&self, cik: u64) -> Result<CompanyInfo> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Submissions {
            name: String,
            #[serde(default)]
            sic: String,
            #[serde(default)]
            sic_description: String,
            #[serde(default)]
            state_of_incorporation: String,
        }
        let s: Submissions = self
            .get_json(&format!("{SUBMISSIONS_URL}/CIK{cik:010}.json"))
            .await?;
        Ok(CompanyInfo {
            name: s.name,
            sic: s.sic.parse().ok(),
            sic_description: s.sic_description,
            incorporated_in: s.state_of_incorporation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_means_a_country_code() {
        let info = |code: &str| CompanyInfo {
            name: String::new(),
            sic: None,
            sic_description: String::new(),
            incorporated_in: code.into(),
        };
        assert!(!info("DE").is_foreign());
        assert!(info("E9").is_foreign());
        assert!(info("D5").is_foreign());
        assert!(!info("").is_foreign());
    }
}
