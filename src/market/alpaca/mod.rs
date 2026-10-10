//! A minimal client for Alpaca's REST APIs: market data and paper trading.
//!
//! Written in-house rather than pulling in a third-party SDK: Alpaca has no
//! official Rust crate, and this keeps the supply chain to reqwest + serde.
//! Must run on the tokio runtime; see [`crate::runtime`].

mod data;
mod trading;

pub use data::*;
pub use trading::*;

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

const DEFAULT_DATA_URL: &str = "https://data.alpaca.markets";

/// Warren trades against paper accounts only. This is deliberately not
/// configurable from the environment, so a stray variable can't point order
/// placement at a live account.
const PAPER_TRADING_URL: &str = "https://paper-api.alpaca.markets";

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

    fn data_request(&self, method: Method, path: &str) -> RequestBuilder {
        self.request(method, &self.data_url, path)
    }

    fn trading_request(&self, method: Method, path: &str) -> RequestBuilder {
        self.request(method, PAPER_TRADING_URL, path)
    }

    fn request(&self, method: Method, base_url: &str, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{base_url}{path}"))
            .header("APCA-API-KEY-ID", &self.key_id)
            .header("APCA-API-SECRET-KEY", &self.secret_key)
    }
}

/// Rate-limited requests are retried this many times before giving up.
const RATE_LIMIT_RETRIES: u32 = 4;

/// Send `request`, failing with Alpaca's error message on a non-2xx reply.
/// Requests rejected by the rate limit (200 a minute on the free plan) wait
/// for the limit to reset and try again.
async fn send(request: RequestBuilder) -> Result<Response> {
    let (client, request) = request.build_split();
    let request = request?;
    let what = format!("{} {}", request.method(), request.url().path());
    let mut attempt = 0;
    let response = loop {
        let this_try = request
            .try_clone()
            .context("request body can't be resent")?;
        let response = client
            .execute(this_try)
            .await
            .with_context(|| format!("Alpaca {what} failed"))?;
        if response.status() != StatusCode::TOO_MANY_REQUESTS || attempt == RATE_LIMIT_RETRIES {
            break response;
        }
        attempt += 1;
        tokio::time::sleep(rate_limit_wait(&response, attempt)).await;
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("Alpaca {what} returned {status}: {body}");
    }
    Ok(response)
}

/// Until the `X-RateLimit-Reset` time (Unix seconds) if Alpaca sent one,
/// otherwise a backoff; at most a minute, the limit's window.
fn rate_limit_wait(response: &Response, attempt: u32) -> Duration {
    let reset = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok()?.parse::<i64>().ok())
        .map(|reset| reset - chrono::Utc::now().timestamp() + 1);
    let seconds = match reset {
        Some(s) if s > 0 => s as u64,
        _ => 2u64.pow(attempt),
    };
    Duration::from_secs(seconds.min(60))
}

async fn send_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    let response = send(request).await?;
    let what = response.url().path().to_string();
    response
        .json()
        .await
        .with_context(|| format!("decoding Alpaca response from {what}"))
}

/// Alpaca's trading API encodes numbers as strings, e.g. `"cash": "1000.50"`.
mod string_number {
    use serde::{Deserialize, Deserializer, de::Error};

    pub fn f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }

    pub fn option_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|s| s.parse().map_err(D::Error::custom))
            .transpose()
    }
}
