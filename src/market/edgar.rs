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
use serde::{Deserialize, Serialize};
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

/// Forms with financial results or material events.
const FINANCIAL_FORMS: [&str; 10] = [
    "10-K", "10-Q", "8-K", "10-K/A", "10-Q/A", "8-K/A", "20-F", "6-K", "40-F", "S-1",
];

/// One filing in a company's EDGAR history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Filing {
    pub form: String,
    pub filed: chrono::NaiveDate,
    /// The period the filing covers, if any.
    pub report_date: Option<chrono::NaiveDate>,
    pub accession: String,
    #[serde(skip)]
    pub primary_document: String,
    pub description: String,
    /// 8-K item numbers, e.g. "2.02,9.01".
    pub items: String,
}

impl Filing {
    /// An 8-K announcing results (item 2.02).
    pub fn is_earnings(&self) -> bool {
        self.form.starts_with("8-K") && self.items.split(',').any(|i| i.trim() == "2.02")
    }
}

/// Plain text from an HTML filing: tags, scripts, styles, and inline XBRL
/// headers dropped, common entities decoded, whitespace collapsed, with
/// block elements on their own lines.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip whole elements whose content isn't text.
            let rest = &lower[i..];
            let skip = ["<script", "<style", "<ix:header"]
                .iter()
                .find(|tag| rest.starts_with(**tag));
            if let Some(tag) = skip {
                let close = format!("</{}", &tag[1..]);
                i += match rest.find(&close) {
                    // Past the closing tag's `>`.
                    Some(at) => rest[at..].find('>').map_or(rest.len(), |gt| at + gt + 1),
                    None => rest.len(),
                };
                continue;
            }
            let end = rest.find('>').map_or(rest.len(), |at| at + 1);
            let tag = &rest[..end];
            let block = [
                "<p", "<br", "<div", "<tr", "<li", "<h1", "<h2", "<h3", "<h4", "</p", "</div",
                "</tr", "</table",
            ];
            if block.iter().any(|b| tag.starts_with(b)) {
                out.push('\n');
            } else if tag.starts_with("<td") || tag.starts_with("<th") {
                out.push(' ');
            }
            i += end;
            continue;
        }
        let next = html[i..].find('<').map_or(html.len(), |at| i + at);
        out.push_str(&html[i..next]);
        i = next;
    }
    let decoded = decode_entities(&out);
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decode HTML entities: numeric ones (`&#58;`, `&#x2019;`) and the
/// common named ones. Unknown entities are left as they are.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "nbsp" => Some(' '),
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "rsquo" | "lsquo" => Some('\''),
            "rdquo" | "ldquo" => Some('"'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "bull" => Some('•'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32)
                .map(|c| if c == '\u{a0}' { ' ' } else { c }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The press release's file name from a filing's index page: the EX-99.1
/// row's document, or the first EX-99.
fn press_release(index_html: &str) -> Option<String> {
    let rows: Vec<(String, String)> = index_html
        .split("<tr")
        .skip(1)
        .filter_map(|row| {
            // The document link, then the row's cells as text.
            let href = row.find("href=\"")? + 6;
            let link = &row[href..row[href..].find('"')? + href];
            let name = link.rsplit('/').next()?.to_string();
            let text = html_to_text(&format!("<tr{row}"));
            Some((name, text))
        })
        .collect();
    let is_text = |name: &String| {
        let lower = name.to_lowercase();
        lower.ends_with(".htm") || lower.ends_with(".html") || lower.ends_with(".txt")
    };
    rows.iter()
        .find(|(name, text)| text.contains("EX-99.1") && is_text(name))
        .or_else(|| {
            rows.iter()
                .find(|(name, text)| text.contains("EX-99") && is_text(name))
        })
        .map(|(name, _)| name.clone())
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

    /// The company's recent filings of financially relevant forms, newest
    /// first (insider and ownership forms are left out).
    pub async fn filings(&self, cik: u64) -> Result<Vec<Filing>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Recent {
            accession_number: Vec<String>,
            filing_date: Vec<String>,
            report_date: Vec<String>,
            form: Vec<String>,
            items: Vec<String>,
            primary_document: Vec<String>,
            primary_doc_description: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Filings {
            recent: Recent,
        }
        #[derive(Deserialize)]
        struct Submissions {
            filings: Filings,
        }
        let s: Submissions = self
            .get_json(&format!("{SUBMISSIONS_URL}/CIK{cik:010}.json"))
            .await?;
        let r = s.filings.recent;
        Ok((0..r.form.len())
            .filter(|&i| FINANCIAL_FORMS.contains(&r.form[i].as_str()))
            .filter_map(|i| {
                Some(Filing {
                    form: r.form[i].clone(),
                    filed: r.filing_date[i].parse().ok()?,
                    report_date: r.report_date[i].parse().ok(),
                    accession: r.accession_number[i].clone(),
                    primary_document: r.primary_document[i].clone(),
                    description: r.primary_doc_description[i].clone(),
                    items: r.items[i].clone(),
                })
            })
            .collect())
    }

    /// A filing's readable text: for an earnings 8-K (item 2.02), its press
    /// release exhibit (99.1) when there is one, else the primary document.
    pub async fn filing_text(&self, cik: u64, filing: &Filing) -> Result<String> {
        let folder = format!(
            "https://www.sec.gov/Archives/edgar/data/{cik}/{}",
            filing.accession.replace('-', "")
        );
        let mut document = filing.primary_document.clone();
        if filing.is_earnings() {
            // The index page labels each document's type; the press release
            // is exhibit 99.1 (some filers name it, e.g., q2fy27pr.htm).
            let index = self
                .get_text(&format!("{folder}/{}-index.html", filing.accession))
                .await?;
            if let Some(exhibit) = press_release(&index) {
                document = exhibit;
            }
        }
        let html = self.get_text(&format!("{folder}/{document}")).await?;
        Ok(html_to_text(&html))
    }

    async fn get_text(&self, url: &str) -> Result<String> {
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
        Ok(response.text().await?)
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
    fn html_becomes_readable_text() {
        let html = "<html><head><style>p{color:red}</style></head><body>\
            <ix:header><ix:hidden>junk</ix:hidden></ix:header>\
            <p>Revenue&nbsp;was <b>$96.2</b> billion,</p><p>up 106%.</p>\
            <table><tr><td>Q2</td><td>$96.2</td></tr></table>\
            <script>var x = 1;</script></body></html>";
        assert_eq!(
            html_to_text(html),
            "Revenue was $96.2 billion,\nup 106%.\nQ2 $96.2"
        );
        assert_eq!(
            decode_entities("as follows&#58; &#8226;Revenue &#x2014; &amp; &bogus; 5 &lt 6"),
            "as follows: •Revenue — & &bogus; 5 &lt 6"
        );
    }

    #[test]
    fn finds_the_press_release_in_a_filing_index() {
        let index = r#"<table><tr><th>Seq</th><th>Type</th></tr>
            <tr><td>1</td><td>8-K</td><td><a href="/Archives/edgar/data/1/2/nvda-20260826.htm">nvda-20260826.htm</a></td><td>8-K</td></tr>
            <tr class="evenRow"><td>2</td><td>EX-99.1</td><td><a href="/Archives/edgar/data/1/2/q2fy27pr.htm">q2fy27pr.htm</a></td><td>EX-99.1</td></tr>
            <tr><td>3</td><td>EX-99.2</td><td><a href="/Archives/edgar/data/1/2/cfo.htm">cfo.htm</a></td><td>EX-99.2</td></tr>
            </table>"#;
        assert_eq!(press_release(index).as_deref(), Some("q2fy27pr.htm"));
        assert_eq!(press_release("<table></table>"), None);
    }

    #[test]
    fn earnings_filings_are_8ks_with_item_202() {
        let filing = |form: &str, items: &str| Filing {
            form: form.into(),
            filed: "2026-08-26".parse().unwrap(),
            report_date: None,
            accession: String::new(),
            primary_document: String::new(),
            description: String::new(),
            items: items.into(),
        };
        assert!(filing("8-K", "2.02,9.01").is_earnings());
        assert!(!filing("8-K", "5.02").is_earnings());
        assert!(!filing("10-Q", "").is_earnings());
    }

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
