//! A minimal client for Alpaca's market data REST API (v2 stocks).
//!
//! Written in-house rather than pulling in a third-party SDK: Alpaca has no
//! official Rust crate, and this keeps the supply chain to reqwest + serde.
//! Must run on the tokio runtime; see [`crate::runtime`].
//!
//! Docs: <https://docs.alpaca.markets/reference/stocksnapshots-1>

// Bars and the full quote/trade fields are for upcoming charts and Claude tools.
#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

const DEFAULT_DATA_URL: &str = "https://data.alpaca.markets";

pub struct AlpacaClient {
    http: reqwest::Client,
    key_id: String,
    secret_key: String,
    data_url: String,
    /// `iex` (free), `sip` (paid), or `delayed_sip`.
    feed: String,
}

impl AlpacaClient {
    /// Reads `APCA_API_KEY_ID` and `APCA_API_SECRET_KEY`, plus the optional
    /// `APCA_API_DATA_URL` and `APCA_DATA_FEED` (defaults to `iex`).
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
        Ok(Self {
            http: reqwest::Client::new(),
            key_id: var("APCA_API_KEY_ID")?,
            secret_key: var("APCA_API_SECRET_KEY")?,
            data_url: var("APCA_API_DATA_URL").unwrap_or_else(|_| DEFAULT_DATA_URL.into()),
            feed: var("APCA_DATA_FEED").unwrap_or_else(|_| "iex".into()),
        })
    }

    /// Latest trade, quote, and minute/daily bars for each symbol. Symbols
    /// Alpaca doesn't recognize are absent from the result.
    pub async fn snapshots(&self, symbols: &[String]) -> Result<HashMap<String, Snapshot>> {
        self.get(
            "/v2/stocks/snapshots",
            &[
                ("symbols", symbols.join(",").as_str()),
                ("feed", &self.feed),
            ],
        )
        .await
    }

    /// Historical bars for `symbol` from `start` (RFC 3339 or `YYYY-MM-DD`),
    /// following pagination. `timeframe` is e.g. `1Min`, `1Hour`, `1Day`.
    pub async fn bars(&self, symbol: &str, timeframe: &str, start: &str) -> Result<Vec<Bar>> {
        #[derive(Deserialize)]
        struct Page {
            bars: Option<Vec<Bar>>,
            next_page_token: Option<String>,
        }

        let path = format!("/v2/stocks/{symbol}/bars");
        let mut bars = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query = vec![
                ("timeframe", timeframe),
                ("start", start),
                ("feed", &self.feed),
                ("limit", "10000"),
            ];
            if let Some(token) = page_token.as_deref() {
                query.push(("page_token", token));
            }
            let page: Page = self.get(&path, &query).await?;
            bars.extend(page.bars.unwrap_or_default());
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(bars),
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let response = self
            .http
            .get(format!("{}{path}", self.data_url))
            .header("APCA-API-KEY-ID", &self.key_id)
            .header("APCA-API-SECRET-KEY", &self.secret_key)
            .query(query)
            .send()
            .await
            .with_context(|| format!("GET {path} failed"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Alpaca GET {path} returned {status}: {body}");
        }
        response
            .json()
            .await
            .with_context(|| format!("decoding Alpaca GET {path}"))
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
    pub timestamp: String,
    #[serde(rename = "p")]
    pub price: f64,
    #[serde(rename = "s")]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quote {
    #[serde(rename = "t")]
    pub timestamp: String,
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
    pub timestamp: String,
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
