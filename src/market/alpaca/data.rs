//! Market data: snapshots and historical bars (v2 stocks).
//!
//! Docs: <https://docs.alpaca.markets/reference/stocksnapshots-1>

// The full quote/trade fields are for upcoming Claude tools.
#![allow(dead_code)]

use std::{collections::HashMap, fmt};

use anyhow::{Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::{AlpacaClient, send_json};

impl AlpacaClient {
    /// Latest trade, quote, and minute/daily bars for each symbol. Symbols
    /// Alpaca doesn't recognize are absent from the result.
    pub async fn snapshots(&self, symbols: &[String]) -> Result<HashMap<String, Snapshot>> {
        let request = self
            .data_request(Method::GET, "/v2/stocks/snapshots")
            .query(&[
                ("symbols", symbols.join(",").as_str()),
                ("feed", &self.feed),
            ]);
        send_json(request).await
    }

    /// Historical bars for `symbol` since `start`, oldest first, following
    /// pagination.
    pub async fn bars(
        &self,
        symbol: &str,
        timeframe: Timeframe,
        start: DateTime<Utc>,
    ) -> Result<Vec<Bar>> {
        #[derive(Deserialize)]
        struct Page {
            bars: Option<Vec<Bar>>,
            next_page_token: Option<String>,
        }

        if !timeframe.is_valid() {
            bail!("Alpaca does not support a {timeframe:?} bar timeframe");
        }
        let timeframe = timeframe.to_string();
        let path = format!("/v2/stocks/{symbol}/bars");
        let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut bars = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query = vec![
                ("timeframe", timeframe.as_str()),
                ("start", start.as_str()),
                ("feed", &self.feed),
                ("limit", "10000"),
            ];
            if let Some(token) = page_token.as_deref() {
                query.push(("page_token", token));
            }
            let page: Page = send_json(self.data_request(Method::GET, &path).query(&query)).await?;
            bars.extend(page.bars.unwrap_or_default());
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(bars),
            }
        }
    }

    /// Recent news for `symbols` since `start`, newest first, without
    /// article bodies.
    pub async fn news(
        &self,
        symbols: &[String],
        start: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<NewsArticle>> {
        #[derive(Deserialize)]
        struct Page {
            news: Vec<NewsArticle>,
        }
        let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);
        let request = self
            .data_request(Method::GET, "/v1beta1/news")
            .query(&[
                ("symbols", symbols.join(",").as_str()),
                ("start", start.as_str()),
                ("sort", "desc"),
                ("include_content", "false"),
            ])
            .query(&[("limit", limit.min(50))]);
        Ok(send_json::<Page>(request).await?.news)
    }

    /// The most actively traded stocks by volume, today.
    pub async fn most_actives(&self, top: u32) -> Result<Vec<ActiveStock>> {
        #[derive(Deserialize)]
        struct Page {
            most_actives: Vec<ActiveStock>,
        }
        let request = self
            .data_request(Method::GET, "/v1beta1/screener/stocks/most-actives")
            .query(&[("by", "volume")])
            .query(&[("top", top.min(100))]);
        Ok(send_json::<Page>(request).await?.most_actives)
    }

    /// Today's top gainers and losers by percent change.
    pub async fn movers(&self, top: u32) -> Result<Movers> {
        let request = self
            .data_request(Method::GET, "/v1beta1/screener/stocks/movers")
            .query(&[("top", top.min(50))]);
        send_json(request).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsArticle {
    pub headline: String,
    #[serde(default)]
    pub summary: String,
    pub source: String,
    pub symbols: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveStock {
    pub symbol: String,
    pub volume: f64,
    pub trade_count: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Movers {
    pub gainers: Vec<Mover>,
    pub losers: Vec<Mover>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mover {
    pub symbol: String,
    pub price: f64,
    pub change: f64,
    pub percent_change: f64,
}

/// Bar aggregation period. Alpaca accepts 1–59 minutes, 1–23 hours, one day,
/// one week, or 1, 2, 3, 4, 6, or 12 months.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeframe {
    Minutes(u8),
    Hours(u8),
    Day,
    Week,
    Months(u8),
}

impl Timeframe {
    pub fn is_valid(self) -> bool {
        match self {
            Self::Minutes(n) => (1..=59).contains(&n),
            Self::Hours(n) => (1..=23).contains(&n),
            Self::Day | Self::Week => true,
            Self::Months(n) => matches!(n, 1 | 2 | 3 | 4 | 6 | 12),
        }
    }
}

/// The query-string form Alpaca expects, e.g. `5Min`, `1Hour`, `1Day`.
impl fmt::Display for Timeframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Minutes(n) => write!(f, "{n}Min"),
            Self::Hours(n) => write!(f, "{n}Hour"),
            Self::Day => f.write_str("1Day"),
            Self::Week => f.write_str("1Week"),
            Self::Months(n) => write!(f, "{n}Month"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub latest_trade: Option<Trade>,
    pub latest_quote: Option<Quote>,
    pub minute_bar: Option<Bar>,
    pub daily_bar: Option<Bar>,
    pub prev_daily_bar: Option<Bar>,
}

impl Snapshot {
    pub fn last_price(&self) -> Option<f64> {
        self.latest_trade.as_ref().map(|trade| trade.price)
    }

    /// Percent change of the last trade versus the previous day's close.
    pub fn change_percent(&self) -> Option<f64> {
        let prev_close = self.prev_daily_bar.as_ref()?.close;
        Some((self.last_price()? - prev_close) / prev_close * 100.0)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trade {
    #[serde(rename = "t")]
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "p")]
    pub price: f64,
    #[serde(rename = "s")]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quote {
    #[serde(rename = "t")]
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "bp")]
    pub bid_price: f64,
    #[serde(rename = "bs")]
    pub bid_size: u64,
    #[serde(rename = "ap")]
    pub ask_price: f64,
    #[serde(rename = "as")]
    pub ask_size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bar {
    #[serde(rename = "t")]
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "o")]
    pub open: f64,
    #[serde(rename = "h")]
    pub high: f64,
    #[serde(rename = "l")]
    pub low: f64,
    #[serde(rename = "c")]
    pub close: f64,
    #[serde(rename = "v")]
    pub volume: u64,
    #[serde(rename = "n")]
    pub trade_count: u64,
    #[serde(rename = "vw")]
    pub vwap: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_snapshot() {
        let json = r#"{
            "AAPL": {
                "latestTrade": {"t": "2026-10-09T19:59:59.5Z", "x": "V", "p": 231.5, "s": 100, "c": ["@"], "i": 1, "z": "C"},
                "latestQuote": {"t": "2026-10-09T19:59:59.6Z", "ax": "V", "ap": 231.55, "as": 2, "bx": "V", "bp": 231.45, "bs": 3, "c": ["R"], "z": "C"},
                "minuteBar": {"t": "2026-10-09T19:59:00Z", "o": 231.4, "h": 231.6, "l": 231.3, "c": 231.5, "v": 1200, "n": 40, "vw": 231.47},
                "dailyBar": {"t": "2026-10-09T04:00:00Z", "o": 228.0, "h": 232.0, "l": 227.5, "c": 231.5, "v": 900000, "n": 12000, "vw": 230.1},
                "prevDailyBar": {"t": "2026-10-08T04:00:00Z", "o": 226.0, "h": 229.0, "l": 225.0, "c": 225.0, "v": 850000, "n": 11000, "vw": 227.3}
            },
            "NEWCO": {"latestTrade": null, "latestQuote": null, "minuteBar": null, "dailyBar": null, "prevDailyBar": null}
        }"#;
        let snapshots: HashMap<String, Snapshot> = serde_json::from_str(json).unwrap();

        let aapl = &snapshots["AAPL"];
        assert_eq!(aapl.last_price(), Some(231.5));
        let change = aapl.change_percent().unwrap();
        assert!((change - 2.888_888).abs() < 1e-4, "{change}");

        assert_eq!(snapshots["NEWCO"].change_percent(), None);
    }

    #[test]
    fn timeframe_query_strings() {
        assert_eq!(Timeframe::Minutes(5).to_string(), "5Min");
        assert_eq!(Timeframe::Hours(1).to_string(), "1Hour");
        assert_eq!(Timeframe::Day.to_string(), "1Day");
        assert_eq!(Timeframe::Months(3).to_string(), "3Month");
        assert!(!Timeframe::Minutes(60).is_valid());
        assert!(!Timeframe::Months(5).is_valid());
    }

    #[test]
    fn decodes_bars_page() {
        let json = r#"{"bars": [{"t": "2026-10-08T04:00:00Z", "o": 1.0, "h": 2.0, "l": 0.5, "c": 1.5, "v": 10, "n": 2, "vw": 1.2}],
                       "symbol": "AAPL", "next_page_token": null}"#;
        #[derive(Deserialize)]
        struct Page {
            bars: Vec<Bar>,
        }
        let page: Page = serde_json::from_str(json).unwrap();
        assert_eq!(page.bars[0].close, 1.5);
    }

    /// Hits the live API; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_snapshots() {
        dotenvy::dotenv().ok();
        let client = AlpacaClient::from_env().unwrap();
        let snapshots = crate::runtime::RUNTIME
            .block_on(client.snapshots(&["AAPL".into(), "SPY".into()]))
            .unwrap();
        println!("{snapshots:#?}");
        assert!(snapshots["AAPL"].last_price().is_some());
    }
}
