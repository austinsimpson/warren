//! The cash sweep: an agent's idle cash parked in a Treasury-bill fund
//! (SGOV by default) so it earns T-bill yield instead of nothing, as a
//! cautious investor's cash would. The code manages it, not the model: idle
//! cash above a small buffer is swept in after each executor run, and buys
//! sell sweep shares first when cash runs short. Sweep shares count as cash
//! for every limit and aren't positions.

use anyhow::{Context as _, Result};

use super::{AgentCtx, book::Book};
use crate::market::alpaca::{NewOrder, OrderSide};

/// The default fund: iShares 0-3 Month Treasury Bond ETF.
pub const DEFAULT_FUND: &str = "SGOV";
/// Cash kept out of the sweep, as a percent of the budget.
const BUFFER_PERCENT: f64 = 2.0;
/// Sweeps smaller than this aren't worth an order.
const MIN_SWEEP: f64 = 500.0;

async fn price(ctx: &AgentCtx, fund: &str) -> Result<f64> {
    ctx.alpaca
        .snapshots(&[fund.to_string()])
        .await?
        .get(fund)
        .and_then(|s| s.last_price())
        .with_context(|| format!("no price for {fund}"))
}

async fn submit(ctx: &AgentCtx, fund: &str, side: OrderSide, qty: u32) -> Result<()> {
    let order = NewOrder::market(fund, side, qty).with_client_order_id(ctx.strategy.new_order_id());
    ctx.alpaca.submit_order(&order).await?;
    Ok(())
}

/// Sell enough sweep shares that `needed` dollars of cash are free for a
/// buy. Does nothing without a sweep or when cash already covers it.
pub async fn fund(ctx: &AgentCtx, book: &Book, needed: f64) -> Result<()> {
    let (Some(fund), Some(held)) = (&ctx.strategy.cash_sweep, &book.sweep) else {
        return Ok(());
    };
    let shortfall = needed - book.idle_cash();
    if shortfall <= 0.0 {
        return Ok(());
    }
    let price = match held.price {
        Some(p) => p,
        None => price(ctx, fund).await?,
    };
    let qty = ((shortfall / price).ceil() as u32).min(held.qty.floor() as u32);
    if qty > 0 {
        submit(ctx, fund, OrderSide::Sell, qty).await?;
        ctx.status(format!("Sold {qty} {fund} to fund a buy"));
    }
    Ok(())
}

/// Park idle cash above the buffer in the sweep fund.
pub async fn park_idle_cash(ctx: &AgentCtx) -> Result<()> {
    let Some(fund) = &ctx.strategy.cash_sweep else {
        return Ok(());
    };
    let book = Book::load(&ctx.alpaca, &ctx.strategy).await?;
    let excess = book.idle_cash() - book.budget * BUFFER_PERCENT / 100.0;
    if excess < MIN_SWEEP {
        return Ok(());
    }
    let price = price(ctx, fund).await?;
    let qty = (excess / price).floor() as u32;
    if qty > 0 {
        submit(ctx, fund, OrderSide::Buy, qty).await?;
        ctx.status(format!(
            "Swept ${:.0} of idle cash into {qty} {fund}",
            qty as f64 * price
        ));
    }
    Ok(())
}
