//! Paper trading: account, positions, and orders.
//!
//! Docs: <https://docs.alpaca.markets/reference/getaccount-1>
//!
//! Money values are `f64`, which is fine for display. Revisit before doing
//! arithmetic on balances that has to be exact.

// Fields and limit orders the UI doesn't surface yet are for upcoming Claude tools.
#![allow(dead_code)]

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::{AlpacaClient, send, send_json, string_number};

impl AlpacaClient {
    pub async fn account(&self) -> Result<Account> {
        send_json(self.trading_request(Method::GET, "/v2/account")).await
    }

    pub async fn positions(&self) -> Result<Vec<Position>> {
        send_json(self.trading_request(Method::GET, "/v2/positions")).await
    }

    /// The most recent orders of any status, newest first.
    pub async fn recent_orders(&self, limit: u32) -> Result<Vec<Order>> {
        let request = self
            .trading_request(Method::GET, "/v2/orders")
            .query(&[("status", "all"), ("direction", "desc")])
            .query(&[("limit", limit)]);
        send_json(request).await
    }

    pub async fn submit_order(&self, order: &NewOrder) -> Result<Order> {
        send_json(self.trading_request(Method::POST, "/v2/orders").json(order)).await
    }

    pub async fn order(&self, order_id: &str) -> Result<Order> {
        let path = format!("/v2/orders/{order_id}");
        send_json(self.trading_request(Method::GET, &path)).await
    }

    /// Whether the market is open now, and the next open and close.
    pub async fn clock(&self) -> Result<Clock> {
        send_json(self.trading_request(Method::GET, "/v2/clock")).await
    }

    /// Tradability details for one symbol.
    pub async fn asset(&self, symbol: &str) -> Result<Asset> {
        let path = format!("/v2/assets/{symbol}");
        send_json(self.trading_request(Method::GET, &path)).await
    }

    /// Daily account equity since `start`, oldest first. Days before the
    /// account was funded are left out.
    pub async fn equity_history(&self, start: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, f64)>> {
        #[derive(Deserialize)]
        struct History {
            timestamp: Vec<i64>,
            equity: Vec<Option<f64>>,
        }
        let start = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let request = self
            .trading_request(Method::GET, "/v2/account/portfolio/history")
            .query(&[("timeframe", "1D"), ("start", start.as_str())]);
        let history: History = send_json(request).await?;
        Ok(history
            .timestamp
            .into_iter()
            .zip(history.equity)
            .filter_map(|(at, equity)| {
                let equity = equity.filter(|e| *e > 0.0)?;
                Some((DateTime::from_timestamp(at, 0)?, equity))
            })
            .collect())
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let path = format!("/v2/orders/{order_id}");
        send(self.trading_request(Method::DELETE, &path)).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Clock {
    pub is_open: bool,
    pub next_open: DateTime<Utc>,
    pub next_close: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub symbol: String,
    pub name: String,
    /// e.g. `us_equity`.
    pub class: String,
    /// e.g. `NASDAQ`, `NYSE`, `OTC`.
    pub exchange: String,
    /// `active` or `inactive`.
    pub status: String,
    pub tradable: bool,
    pub shortable: bool,
    pub fractionable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub status: String,
    #[serde(deserialize_with = "string_number::f64")]
    pub cash: f64,
    #[serde(deserialize_with = "string_number::f64")]
    pub buying_power: f64,
    #[serde(deserialize_with = "string_number::f64")]
    pub equity: f64,
    /// Equity at the previous trading day's close.
    #[serde(deserialize_with = "string_number::f64")]
    pub last_equity: f64,
    pub trading_blocked: bool,
}

impl Account {
    pub fn day_change(&self) -> f64 {
        self.equity - self.last_equity
    }

    pub fn day_change_percent(&self) -> Option<f64> {
        (self.last_equity != 0.0).then(|| self.day_change() / self.last_equity * 100.0)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    pub symbol: String,
    /// Negative for short positions.
    #[serde(deserialize_with = "string_number::f64")]
    pub qty: f64,
    #[serde(deserialize_with = "string_number::f64")]
    pub avg_entry_price: f64,
    #[serde(deserialize_with = "string_number::option_f64")]
    pub current_price: Option<f64>,
    #[serde(deserialize_with = "string_number::option_f64")]
    pub market_value: Option<f64>,
    #[serde(deserialize_with = "string_number::option_f64")]
    pub unrealized_pl: Option<f64>,
    /// Unrealized P/L as a fraction of cost basis (0.05 = 5%).
    #[serde(deserialize_with = "string_number::option_f64")]
    pub unrealized_plpc: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn label(self) -> &'static str {
        match self {
            Self::Buy => "Buy",
            Self::Sell => "Sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
    Stop,
    StopLimit,
    TrailingStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Day,
    Gtc,
    Opg,
    Cls,
    Ioc,
    Fok,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Order {
    pub id: String,
    pub symbol: String,
    /// Absent for notional (dollar-amount) orders.
    #[serde(deserialize_with = "string_number::option_f64")]
    pub qty: Option<f64>,
    #[serde(deserialize_with = "string_number::f64")]
    pub filled_qty: f64,
    #[serde(deserialize_with = "string_number::option_f64")]
    pub filled_avg_price: Option<f64>,
    #[serde(deserialize_with = "string_number::option_f64")]
    pub limit_price: Option<f64>,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    /// e.g. `new`, `accepted`, `partially_filled`, `filled`, `canceled`.
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl Order {
    /// Whether the order can still fill (and so can be canceled).
    pub fn is_open(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "filled" | "canceled" | "expired" | "rejected" | "replaced"
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NewOrder {
    pub symbol: String,
    /// Whole shares; Alpaca takes quantities as strings.
    pub qty: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<String>,
}

impl NewOrder {
    /// A day market order for whole shares.
    pub fn market(symbol: impl Into<String>, side: OrderSide, qty: u32) -> Self {
        Self {
            symbol: symbol.into(),
            qty: qty.to_string(),
            side,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            limit_price: None,
        }
    }

    /// A day limit order for whole shares.
    pub fn limit(symbol: impl Into<String>, side: OrderSide, qty: u32, limit_price: f64) -> Self {
        Self {
            order_type: OrderType::Limit,
            limit_price: Some(format!("{limit_price:.2}")),
            ..Self::market(symbol, side, qty)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_account() {
        let json = r#"{"id": "x", "account_number": "PA1", "status": "ACTIVE", "currency": "USD",
            "cash": "50000.25", "buying_power": "200000", "equity": "101000.5", "last_equity": "100000",
            "trading_blocked": false, "pattern_day_trader": false}"#;
        let account: Account = serde_json::from_str(json).unwrap();
        assert_eq!(account.cash, 50000.25);
        assert_eq!(account.day_change(), 1000.5);
        assert!((account.day_change_percent().unwrap() - 1.0005).abs() < 1e-9);
    }

    #[test]
    fn decodes_position() {
        let json = r#"{"asset_id": "x", "symbol": "AAPL", "exchange": "NASDAQ", "asset_class": "us_equity",
            "qty": "10", "avg_entry_price": "300.5", "side": "long", "market_value": "3358.6",
            "cost_basis": "3005", "unrealized_pl": "353.6", "unrealized_plpc": "0.1176705490848586",
            "current_price": "335.86", "lastday_price": "340.43", "change_today": "-0.0134"}"#;
        let position: Position = serde_json::from_str(json).unwrap();
        assert_eq!(position.qty, 10.0);
        assert_eq!(position.current_price, Some(335.86));
    }

    #[test]
    fn decodes_order() {
        let json = r#"{"id": "abc", "client_order_id": "c", "created_at": "2026-10-09T20:01:02.123456Z",
            "symbol": "AAPL", "qty": "5", "notional": null, "filled_qty": "0", "filled_avg_price": null,
            "order_class": "", "order_type": "limit", "type": "limit", "side": "buy",
            "time_in_force": "day", "limit_price": "1", "stop_price": null, "status": "accepted",
            "extended_hours": false}"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.side, OrderSide::Buy);
        assert_eq!(order.order_type, OrderType::Limit);
        assert_eq!(order.limit_price, Some(1.0));
        assert!(order.is_open());
    }

    #[test]
    fn encodes_new_order() {
        let order = NewOrder::limit("AAPL", OrderSide::Buy, 5, 1.0);
        assert_eq!(
            serde_json::to_value(&order).unwrap(),
            serde_json::json!({"symbol": "AAPL", "qty": "5", "side": "buy", "type": "limit",
                               "time_in_force": "day", "limit_price": "1.00"})
        );
        let market = serde_json::to_value(NewOrder::market("AAPL", OrderSide::Sell, 1)).unwrap();
        assert!(market.get("limit_price").is_none());
    }

    /// Hits the live paper API and places (then cancels) a 1-share buy limit
    /// at $1, far below market so it can't fill. Run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_paper_order_round_trip() {
        dotenvy::dotenv().ok();
        let client = AlpacaClient::from_env().unwrap();
        crate::runtime::RUNTIME.block_on(async {
            let account = client.account().await.unwrap();
            println!("account: {account:?}");
            println!("positions: {:?}", client.positions().await.unwrap());

            let order = client
                .submit_order(&NewOrder::limit("AAPL", OrderSide::Buy, 1, 1.0))
                .await
                .unwrap();
            println!("submitted: {} {}", order.id, order.status);
            assert!(order.is_open());

            client.cancel_order(&order.id).await.unwrap();
            let orders = client.recent_orders(10).await.unwrap();
            let ours = orders.iter().find(|o| o.id == order.id).unwrap();
            println!("after cancel: {}", ours.status);
            assert!(matches!(
                ours.status.as_str(),
                "canceled" | "pending_cancel"
            ));
        });
    }
}
