//! The paper account: summary, positions, recent orders, and the order ticket.

use gpui::{Context, Div, ElementId, FontWeight, SharedString, div, prelude::*, px, rgb};

use crate::agents::book::Book;
use crate::market::alpaca::{Account, NewOrder, Order, OrderSide, OrderType, Position};
use crate::theme::*;
use crate::{Warren, runtime};

const RECENT_ORDER_COUNT: u32 = 20;

#[derive(Default)]
pub struct Portfolio {
    pub account: Option<Account>,
    positions: Vec<Position>,
    orders: Vec<Order>,
    status: SharedString,
    ticket: Ticket,
}

struct Ticket {
    qty: u32,
    /// An order awaiting confirmation. It records its own symbol so changing
    /// the selection can't redirect it.
    pending: Option<PendingOrder>,
    submitting: bool,
    message: SharedString,
}

impl Default for Ticket {
    fn default() -> Self {
        Self {
            qty: 1,
            pending: None,
            submitting: false,
            message: "".into(),
        }
    }
}

#[derive(Clone)]
struct PendingOrder {
    symbol: String,
    side: OrderSide,
    qty: u32,
}

impl Warren {
    /// Symbols with an open position.
    pub fn held_symbols(&self) -> Vec<String> {
        self.portfolio
            .positions
            .iter()
            .map(|p| p.symbol.clone())
            .collect()
    }

    pub fn refresh_portfolio(&mut self, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        // Every agent's book comes from one pull of order history since the
        // oldest agent was created.
        let since = self.agents.iter().map(|a| a.ctx.strategy.created_at).min();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let history = async {
                    match since {
                        Some(since) => {
                            alpaca
                                .orders_after(since - chrono::Duration::minutes(1))
                                .await
                        }
                        None => Ok(Vec::new()),
                    }
                };
                Ok(tokio::try_join!(
                    alpaca.account(),
                    alpaca.positions(),
                    alpaca.recent_orders(RECENT_ORDER_COUNT),
                    history,
                )?)
            })
            .await;
            this.update(cx, |this, cx| {
                if let Ok((_, positions, _, history)) = &result {
                    let prices: std::collections::HashMap<String, f64> = positions
                        .iter()
                        .filter_map(|p| Some((p.symbol.clone(), p.current_price?)))
                        .collect();
                    for agent in &mut this.agents {
                        agent.book = Some(Book::from_orders(&agent.ctx.strategy, history, &prices));
                    }
                }
                let portfolio = &mut this.portfolio;
                match result {
                    Ok((account, positions, orders, _)) => {
                        portfolio.account = Some(account);
                        portfolio.positions = positions;
                        portfolio.orders = orders;
                        portfolio.status = "".into();
                    }
                    Err(err) => portfolio.status = format!("Error: {err:#}").into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Drop an unconfirmed order, e.g. when the selected symbol changes.
    pub fn clear_pending_order(&mut self) {
        self.portfolio.ticket.pending = None;
    }

    fn adjust_qty(&mut self, delta: i64, cx: &mut Context<Self>) {
        let ticket = &mut self.portfolio.ticket;
        ticket.qty = (ticket.qty as i64 + delta).clamp(1, 10_000) as u32;
        ticket.pending = None;
        cx.notify();
    }

    fn stage_order(&mut self, side: OrderSide, cx: &mut Context<Self>) {
        let ticket = &mut self.portfolio.ticket;
        ticket.pending = Some(PendingOrder {
            symbol: self.selected.clone(),
            side,
            qty: ticket.qty,
        });
        ticket.message = "".into();
        cx.notify();
    }

    fn confirm_order(&mut self, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        let ticket = &mut self.portfolio.ticket;
        let Some(pending) = ticket.pending.take() else {
            return;
        };
        ticket.submitting = true;
        ticket.message = "Submitting…".into();
        cx.notify();

        let order = NewOrder::market(pending.symbol, pending.side, pending.qty);
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move { alpaca.submit_order(&order).await }).await;
            this.update(cx, |this, cx| {
                let ticket = &mut this.portfolio.ticket;
                ticket.submitting = false;
                ticket.message = match result {
                    Ok(order) => format!(
                        "{} {} {} — {}",
                        order.side.label(),
                        format_qty(order.qty.unwrap_or_default()),
                        order.symbol,
                        order.status,
                    ),
                    Err(err) => format!("Error: {err:#}"),
                }
                .into();
                this.refresh_portfolio(cx);
                cx.notify();
            })
        })
        .detach();
    }

    fn cancel_order(&mut self, order_id: String, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move { alpaca.cancel_order(&order_id).await }).await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.portfolio.status = format!("Error: {err:#}").into();
                }
                this.refresh_portfolio(cx);
            })
        })
        .detach();
    }

    /// Paper badge plus equity, day P/L, buying power, and cash, in one line.
    pub fn render_account_summary(&self) -> Div {
        let paper_badge = div()
            .px_1p5()
            .rounded_sm()
            .border_1()
            .border_color(tint(WARNING, 0.5))
            .bg(tint(WARNING, 0.1))
            .text_color(rgb(WARNING))
            .text_size(px(TEXT_XS))
            .font_weight(FontWeight::BOLD)
            .child("PAPER");

        let stat = |name: &'static str, value: String| {
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_1()
                .flex_none()
                .child(label(name))
                .child(value)
        };

        let mut summary = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_w_0()
            .overflow_hidden()
            .child(paper_badge);
        if let Some(account) = &self.portfolio.account {
            let (pct, color) = format_change(account.day_change_percent());
            summary = summary
                .child(stat("Equity", format_money(account.equity)))
                .child(
                    stat(
                        "Day",
                        format!("{} {pct}", format_signed_money(account.day_change())),
                    )
                    .text_color(color),
                );
            if account.trading_blocked {
                summary = summary.child(div().text_color(rgb(RED)).child("Trading blocked"));
            }
        }
        if !self.portfolio.status.is_empty() {
            summary = summary.child(
                div()
                    .truncate()
                    .text_size(px(TEXT_XS))
                    .text_color(rgb(WARNING))
                    .child(self.portfolio.status.clone()),
            );
        }
        summary
    }

    /// Quantity stepper with Buy/Sell for the selected symbol, and a confirm
    /// step before anything is sent.
    pub fn render_order_ticket(&self, cx: &mut Context<Self>) -> Div {
        let ticket = &self.portfolio.ticket;
        let last_price = self
            .snapshots
            .get(&self.selected)
            .and_then(|s| s.last_price());

        let controls = if let Some(pending) = &ticket.pending {
            let estimate = last_price
                .map(|p| format!(" (≈ {})", format_money(p * pending.qty as f64)))
                .unwrap_or_default();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().truncate().child(format!(
                    "{} {} {} at market{estimate}?",
                    pending.side.label(),
                    pending.qty,
                    pending.symbol,
                )))
                .child(
                    ghost_button("cancel-pending", "Cancel", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.clear_pending_order();
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("confirm-order", "Confirm", ACCENT)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_order(cx))),
                )
        } else {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(label("Qty"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            ghost_button("qty-down", "−", false)
                                .on_click(cx.listener(|this, _, _, cx| this.adjust_qty(-1, cx))),
                        )
                        .child(
                            div()
                                .w(px(36.))
                                .flex()
                                .justify_center()
                                .child(ticket.qty.to_string()),
                        )
                        .child(
                            ghost_button("qty-up", "+", false)
                                .on_click(cx.listener(|this, _, _, cx| this.adjust_qty(1, cx))),
                        ),
                )
                .children(last_price.map(|p| {
                    div()
                        .text_size(px(TEXT_SM))
                        .text_color(rgb(MUTED))
                        .child(format!("≈ {}", format_money(p * ticket.qty as f64)))
                }))
                .child(div().flex_1())
                .when(!ticket.submitting, |row| {
                    row.child(
                        button(
                            OrderSide::Buy.label(),
                            format!("Buy {}", self.selected),
                            GREEN,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.stage_order(OrderSide::Buy, cx)),
                        ),
                    )
                    .child(
                        button(
                            OrderSide::Sell.label(),
                            format!("Sell {}", self.selected),
                            RED,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.stage_order(OrderSide::Sell, cx)),
                        ),
                    )
                })
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(controls)
            .when(!ticket.message.is_empty(), |ticket_div| {
                ticket_div.child(
                    div()
                        .truncate()
                        .text_size(px(TEXT_XS))
                        .text_color(rgb(MUTED))
                        .child(ticket.message.clone()),
                )
            })
    }

    /// Open positions (click to chart) and recent orders (open ones cancelable).
    pub fn render_positions_and_orders(&self, cx: &mut Context<Self>) -> Div {
        let empty = |text: &'static str| {
            div()
                .px_2()
                .text_size(px(TEXT_SM))
                .text_color(rgb(MUTED))
                .child(text)
        };

        let positions: Vec<_> = self
            .portfolio
            .positions
            .iter()
            .map(|position| {
                let symbol = position.symbol.clone();
                let (pl_pct, pl_color) = format_change(position.unrealized_plpc.map(|f| f * 100.0));
                div()
                    .id(ElementId::Name(format!("position-{symbol}").into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(HOVER)))
                    .child(div().font_weight(FontWeight::MEDIUM).child(symbol.clone()))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(TEXT_SM))
                            .text_color(rgb(MUTED))
                            .child(format!("× {}", format_qty(position.qty))),
                    )
                    .child(div().text_size(px(TEXT_SM)).text_color(pl_color).child(
                        match position.unrealized_pl {
                            Some(pl) => format!("{} {pl_pct}", format_signed_money(pl)),
                            None => pl_pct,
                        },
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(symbol.clone(), cx)))
            })
            .collect();

        let orders: Vec<_> =
            self.portfolio
                .orders
                .iter()
                .map(|order| {
                    let price = match (order.filled_avg_price, order.order_type, order.limit_price)
                    {
                        (Some(fill), _, _) => format!(" @ {fill:.2}"),
                        (None, OrderType::Limit, Some(limit)) => format!(" lim {limit:.2}"),
                        _ => String::new(),
                    };
                    let side_color = match order.side {
                        OrderSide::Buy => GREEN,
                        OrderSide::Sell => RED,
                    };
                    let id = order.id.clone();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(24.))
                        .text_size(px(TEXT_SM))
                        .child(
                            div()
                                .w(px(28.))
                                .text_color(rgb(side_color))
                                .child(order.side.label()),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(format!(
                            "{} {}{price}",
                            format_qty(order.qty.unwrap_or(order.filled_qty)),
                            order.symbol
                        )))
                        .child(
                            div()
                                .text_size(px(TEXT_XS))
                                .text_color(rgb(MUTED))
                                .child(order.status.replace('_', " ")),
                        )
                        .when(order.is_open(), |row| {
                            row.child(
                                ghost_button(
                                    ElementId::Name(format!("cancel-{id}").into()),
                                    "✕",
                                    false,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.cancel_order(id.clone(), cx),
                                )),
                            )
                        })
                })
                .collect();

        div()
            .flex()
            .flex_col()
            .w(px(self.shown.right))
            .flex_none()
            .p_2()
            .gap_1()
            .children(self.portfolio.account.as_ref().map(|account| {
                let row = |name: &'static str, value: String| {
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .px_2()
                        .child(label(name))
                        .child(value)
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .pb_2()
                    .child(div().px_2().pt_1().pb_1().child(label("Account")))
                    .child(row("Buying power", format_money(account.buying_power)))
                    .child(row("Cash", format_money(account.cash)))
            }))
            .child(div().px_2().pt_1().child(label("Positions")))
            .when(positions.is_empty(), |col| col.child(empty("No positions")))
            .children(positions)
            .child(div().px_2().pt_3().child(label("Recent orders")))
            .child(
                div()
                    .id("orders")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(orders.is_empty(), |col| col.child(empty("No orders")))
                    .children(orders),
            )
    }
}

/// `1234.5` → `$1,234.50`.
pub fn format_money(value: f64) -> String {
    let cents = (value.abs() * 100.0).round() as u64;
    let dollars = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, digit) in dollars.chars().enumerate() {
        if i > 0 && (dollars.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if value < 0.0 && cents > 0 { "-" } else { "" };
    format!("{sign}${grouped}.{:02}", cents % 100)
}

/// Like [`format_money`] but always signed: `+$12.00` / `-$3.50`.
fn format_signed_money(value: f64) -> String {
    if value < 0.0 {
        format_money(value)
    } else {
        format!("+{}", format_money(value))
    }
}

/// Whole share counts without a decimal point; fractional ones as given.
fn format_qty(qty: f64) -> String {
    if qty.fract() == 0.0 {
        format!("{qty:.0}")
    } else {
        qty.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formatting() {
        assert_eq!(format_money(0.0), "$0.00");
        assert_eq!(format_money(1234.5), "$1,234.50");
        assert_eq!(format_money(100_000.0), "$100,000.00");
        assert_eq!(format_money(-1_234_567.891), "-$1,234,567.89");
        assert_eq!(format_money(-0.001), "$0.00");
        assert_eq!(format_signed_money(12.0), "+$12.00");
        assert_eq!(format_qty(5.0), "5");
        assert_eq!(format_qty(0.5), "0.5");
    }
}
