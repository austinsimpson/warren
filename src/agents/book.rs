//! A strategy's virtual book within the shared paper account: its cash,
//! positions, and open orders, rebuilt from the orders it placed (tagged by
//! client order ID; see `Strategy::order_prefix`). Alpaca nets every
//! strategy's shares into one position per symbol, so this ledger is what
//! says whose shares are whose.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use crate::market::alpaca::{AlpacaClient, Order, OrderSide};
use crate::strategy::Strategy;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Book {
    pub budget: f64,
    /// Budget less the cost of buys plus the proceeds of sells.
    pub cash: f64,
    /// Cash committed to open buy orders.
    pub reserved: f64,
    pub realized_pl: f64,
    pub positions: Vec<Holding>,
    /// Shares of the strategy's cash sweep (a T-bill fund), held apart from
    /// positions: they count as cash for every limit.
    pub sweep: Option<Holding>,
    /// This strategy's orders that can still fill.
    #[serde(skip)]
    pub open_orders: Vec<Order>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Holding {
    pub symbol: String,
    pub qty: f64,
    pub avg_cost: f64,
    /// Last trade, when known.
    pub price: Option<f64>,
    /// When the order that opened the position was placed.
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

impl Holding {
    pub fn market_value(&self) -> f64 {
        self.qty * self.price.unwrap_or(self.avg_cost)
    }

    pub fn unrealized_pl(&self) -> f64 {
        self.market_value() - self.qty * self.avg_cost
    }
}

impl Book {
    /// The book from `orders` (any order; only this strategy's are used),
    /// oldest first, valuing holdings at `prices`.
    pub fn from_orders(
        strategy: &Strategy,
        orders: &[Order],
        prices: &HashMap<String, f64>,
    ) -> Self {
        let prefix = strategy.order_prefix();
        let mut book = Book {
            budget: strategy.budget,
            cash: strategy.budget,
            ..Book::default()
        };
        // qty, avg cost, opened at
        let mut lots: HashMap<String, (f64, f64, chrono::DateTime<chrono::Utc>)> = HashMap::new();
        for order in orders
            .iter()
            .filter(|o| o.client_order_id.starts_with(&prefix))
        {
            if order.is_open() {
                if order.side == OrderSide::Buy {
                    let remaining = order.qty.unwrap_or(0.0) - order.filled_qty;
                    let price = order
                        .limit_price
                        .or_else(|| prices.get(&order.symbol).copied())
                        .unwrap_or(0.0);
                    book.reserved += remaining.max(0.0) * price;
                }
                book.open_orders.push(order.clone());
            }
            let (Some(fill), qty) = (order.filled_avg_price, order.filled_qty) else {
                continue;
            };
            if qty <= 0.0 {
                continue;
            }
            let lot = lots
                .entry(order.symbol.clone())
                .or_insert((0.0, 0.0, order.created_at));
            match order.side {
                OrderSide::Buy => {
                    if lot.0 <= 1e-9 {
                        lot.2 = order.created_at;
                    }
                    lot.1 = (lot.0 * lot.1 + qty * fill) / (lot.0 + qty);
                    lot.0 += qty;
                    book.cash -= qty * fill;
                }
                OrderSide::Sell => {
                    book.realized_pl += qty * (fill - lot.1);
                    lot.0 -= qty;
                    book.cash += qty * fill;
                }
            }
        }
        book.positions = lots
            .into_iter()
            .filter(|(_, (qty, _, _))| *qty > 1e-9)
            .map(|(symbol, (qty, avg_cost, opened_at))| Holding {
                price: prices.get(&symbol).copied(),
                symbol,
                qty,
                avg_cost,
                opened_at,
            })
            .collect();
        book.positions.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        if let Some(sweep) = &strategy.cash_sweep
            && let Some(i) = book.positions.iter().position(|h| &h.symbol == sweep)
        {
            book.sweep = Some(book.positions.remove(i));
        }
        book
    }

    /// Fetch this strategy's orders and current prices and build its book.
    pub async fn load(alpaca: &AlpacaClient, strategy: &Strategy) -> Result<Self> {
        let orders = alpaca
            .orders_after(strategy.created_at - chrono::Duration::minutes(1))
            .await?;
        let prefix = strategy.order_prefix();
        let mut symbols: Vec<String> = orders
            .iter()
            .filter(|o| o.client_order_id.starts_with(&prefix))
            .map(|o| o.symbol.clone())
            .collect();
        symbols.sort();
        symbols.dedup();
        let prices: HashMap<String, f64> = if symbols.is_empty() {
            HashMap::new()
        } else {
            alpaca
                .snapshots(&symbols)
                .await?
                .into_iter()
                .filter_map(|(symbol, snap)| Some((symbol, snap.last_price()?)))
                .collect()
        };
        Ok(Self::from_orders(strategy, &orders, &prices))
    }

    pub fn equity(&self) -> f64 {
        self.cash
            + self.sweep_value()
            + self
                .positions
                .iter()
                .map(Holding::market_value)
                .sum::<f64>()
    }

    /// Cash not committed to open buy orders, counting the sweep as cash.
    pub fn available_cash(&self) -> f64 {
        self.idle_cash() + self.sweep_value()
    }

    /// Cash not committed to open buy orders, without the sweep.
    pub fn idle_cash(&self) -> f64 {
        self.cash - self.reserved
    }

    pub fn sweep_value(&self) -> f64 {
        self.sweep.as_ref().map_or(0.0, Holding::market_value)
    }

    pub fn held(&self, symbol: &str) -> f64 {
        self.positions
            .iter()
            .find(|h| h.symbol == symbol)
            .map_or(0.0, |h| h.qty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::alpaca::{OrderType, TimeInForce};

    fn order(
        id: &str,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        fill: Option<f64>,
        status: &str,
    ) -> Order {
        Order {
            id: id.into(),
            client_order_id: id.into(),
            symbol: symbol.into(),
            qty: Some(qty),
            filled_qty: if fill.is_some() { qty } else { 0.0 },
            filled_avg_price: fill,
            limit_price: Some(fill.unwrap_or(10.0)),
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Day,
            status: status.into(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn tracks_cash_cost_and_realized_pl() {
        let s = Strategy::new("Alpha", 10_000.0);
        let p = s.order_prefix();
        let orders = vec![
            order(
                &format!("{p}1"),
                "AAA",
                OrderSide::Buy,
                10.0,
                Some(100.0),
                "filled",
            ),
            order(
                &format!("{p}2"),
                "AAA",
                OrderSide::Buy,
                10.0,
                Some(110.0),
                "filled",
            ),
            order(
                &format!("{p}3"),
                "AAA",
                OrderSide::Sell,
                5.0,
                Some(120.0),
                "filled",
            ),
            // Another strategy's order is ignored.
            order(
                "w-beta-1",
                "AAA",
                OrderSide::Buy,
                50.0,
                Some(100.0),
                "filled",
            ),
            // An open buy reserves cash.
            order(&format!("{p}4"), "BBB", OrderSide::Buy, 10.0, None, "new"),
        ];
        let prices = HashMap::from([("AAA".to_string(), 130.0)]);
        let book = Book::from_orders(&s, &orders, &prices);
        assert_eq!(book.held("AAA"), 15.0);
        assert_eq!(book.positions[0].avg_cost, 105.0);
        assert_eq!(book.realized_pl, 75.0);
        assert_eq!(book.cash, 10_000.0 - 1_000.0 - 1_100.0 + 600.0);
        assert_eq!(book.reserved, 100.0);
        assert_eq!(book.equity(), book.cash + 15.0 * 130.0);
        assert_eq!(book.open_orders.len(), 1);
    }

    #[test]
    fn sweep_shares_count_as_cash_not_positions() {
        let mut s = Strategy::new("Boomer", 10_000.0);
        s.cash_sweep = Some("SGOV".into());
        let p = s.order_prefix();
        let orders = vec![
            order(
                &format!("{p}1"),
                "SGOV",
                OrderSide::Buy,
                50.0,
                Some(100.0),
                "filled",
            ),
            order(
                &format!("{p}2"),
                "AAA",
                OrderSide::Buy,
                10.0,
                Some(50.0),
                "filled",
            ),
        ];
        let prices = HashMap::from([("SGOV".to_string(), 100.5), ("AAA".to_string(), 50.0)]);
        let book = Book::from_orders(&s, &orders, &prices);
        assert_eq!(book.positions.len(), 1);
        assert_eq!(book.idle_cash(), 10_000.0 - 5_000.0 - 500.0);
        assert_eq!(book.available_cash(), 4_500.0 + 5_025.0);
        assert_eq!(book.equity(), 4_500.0 + 5_025.0 + 500.0);
    }

    #[test]
    fn closed_positions_drop_out() {
        let s = Strategy::new("Alpha", 1_000.0);
        let p = s.order_prefix();
        let orders = vec![
            order(
                &format!("{p}1"),
                "AAA",
                OrderSide::Buy,
                2.0,
                Some(50.0),
                "filled",
            ),
            order(
                &format!("{p}2"),
                "AAA",
                OrderSide::Sell,
                2.0,
                Some(40.0),
                "filled",
            ),
        ];
        let book = Book::from_orders(&s, &orders, &HashMap::new());
        assert!(book.positions.is_empty());
        assert_eq!(book.realized_pl, -20.0);
        assert_eq!(book.equity(), 980.0);
    }
}
