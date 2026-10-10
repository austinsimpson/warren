//! What exploration found for the selected agent, shown in place of the
//! chart: its market summary, the candidates it picked and why, and the
//! pool of stocks it considered.

use chrono::Local;
use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, SharedString, div, prelude::*, px, rgb,
};
use serde_json::Value;

use crate::Warren;
use crate::theme::*;

/// Figures shown for each pooled stock, in order, with their labels. Rows
/// show whichever their ranking has.
const POOL_FIELDS: [(&str, &str); 11] = [
    ("composite", "rank"),
    ("price", "price"),
    ("revenue_growth_percent", "growth %"),
    ("acceleration_points", "accel"),
    ("price_to_sales", "p/s"),
    ("gross_margin_percent", "gm %"),
    ("return_12_1_percent", "12-1m %"),
    ("percent_below_52w_high", "below high %"),
    ("extension", "ext"),
    ("return_1m_percent", "1m %"),
    ("sector", ""),
];

impl Warren {
    pub fn open_exploration(&mut self, cx: &mut Context<Self>) {
        self.exploration_open = true;
        self.report_open = false;
        cx.notify();
    }

    fn close_exploration(&mut self, cx: &mut Context<Self>) {
        self.exploration_open = false;
        cx.notify();
    }

    /// Select a stock from the view and go to its chart.
    fn pick_from_exploration(&mut self, symbol: String, cx: &mut Context<Self>) {
        self.exploration_open = false;
        self.select(symbol, cx);
        cx.notify();
    }

    pub fn render_exploration(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.exploration_open {
            return None;
        }
        let agent = self.agent()?;
        let candidates = agent.candidates.as_ref();
        let picked: Vec<String> = candidates
            .map(|c| c.candidates.iter().map(|c| c.symbol.clone()).collect())
            .unwrap_or_default();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(TEXT_LG))
                    .font_weight(FontWeight::BOLD)
                    .child(format!("{} exploration", agent.ctx.strategy.name)),
            )
            .children(candidates.map(|c| {
                label(format!(
                    "picked {}",
                    c.generated_at
                        .with_timezone(&Local)
                        .format("%a %b %-d, %-I:%M %p")
                ))
            }))
            .child(div().flex_1())
            .child(
                ghost_button("close-exploration", "Close", false)
                    .on_click(cx.listener(|this, _, _, cx| this.close_exploration(cx))),
            );

        let symbol_button =
            |id: String, symbol: String, cx: &mut Context<Self>| {
                let is_pick = picked.contains(&symbol);
                div()
                    .id(ElementId::Name(id.into()))
                    .flex_none()
                    .w(px(64.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(if is_pick { ACCENT } else { TEXT }))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(ACCENT)))
                    .child(symbol.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_from_exploration(symbol.clone(), cx)
                    }))
            };

        let mut view = div()
            .id("exploration")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .gap_1()
            .child(header);

        match candidates {
            None => {
                view = view.child(
                    prose(
                        "Exploration hasn't run for this agent yet. It runs on the first \
                           cycle once the agent is switched on.",
                    )
                    .text_color(rgb(MUTED)),
                );
            }
            Some(list) => {
                view = view
                    .child(section("market"))
                    .child(prose(list.market_summary.clone()))
                    .child(section("candidates"));
                for c in &list.candidates {
                    view = view.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .pb_1()
                            .child(label(format!("#{}", c.priority)).flex_none().w(px(20.)))
                            .child(symbol_button(
                                format!("pick-{}", c.symbol),
                                c.symbol.clone(),
                                cx,
                            ))
                            .child(prose(c.rationale.clone()).flex_1().min_w_0()),
                    );
                }
            }
        }

        let pool = agent.pool.as_ref();
        if let Some(scan) = pool.and_then(|p| p.scan.as_ref()) {
            let ranked = scan["stocks_ranked"].as_u64().unwrap_or(0);
            view = view
                .child(section(&format!(
                    "pool: top of {ranked} stocks ranked (picks in accent)"
                )))
                .children(scan["ranking"].as_str().map(|r| label(r.to_string())));
            for row in scan["top"].as_array().into_iter().flatten() {
                let Some(symbol) = row["symbol"].as_str() else {
                    continue;
                };
                let cells = POOL_FIELDS.iter().filter_map(|(key, name)| {
                    let value = &row[*key];
                    let text = match value {
                        Value::Number(n) => {
                            let v = n.as_f64()?;
                            if v.fract() == 0.0 && v.abs() < 1e6 {
                                format!("{v:.0}")
                            } else {
                                format!("{v:.1}")
                            }
                        }
                        Value::String(s) => s.clone(),
                        _ => return None,
                    };
                    Some(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .when(!name.is_empty(), |d| d.child(label(name.to_string())))
                            .child(text),
                    )
                });
                view = view.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_x_3()
                        .text_size(px(TEXT_SM))
                        .child(symbol_button(
                            format!("pool-{symbol}"),
                            symbol.to_string(),
                            cx,
                        ))
                        .children(cells),
                );
            }
        }
        if let Some(screen) = pool.and_then(|p| p.screen.as_ref()) {
            view = view.child(section("today's movers"));
            for (key, title) in [
                ("most_active", "most active"),
                ("gainers", "gainers"),
                ("losers", "losers"),
            ] {
                let names: Vec<String> = screen[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| {
                        let symbol = r["symbol"].as_str()?;
                        Some(match r["change_percent"].as_f64() {
                            Some(c) => format!("{symbol} {c:+.1}%"),
                            None => symbol.to_string(),
                        })
                    })
                    .collect();
                if !names.is_empty() {
                    view = view.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(label(title.to_string()).flex_none().w(px(80.)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(TEXT_SM))
                                    .child(names.join("  ")),
                            ),
                    );
                }
            }
        }
        if pool.is_none() && candidates.is_some() {
            view =
                view.child(label("The pool is recorded from the next exploration run on.").pt_2());
        }
        Some(view.child(div().h(px(24.))).into_any_element())
    }
}

fn section(title: &str) -> Div {
    div().pt_3().pb_1().child(label(title.to_string()))
}

fn prose(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(SANS)
        .text_size(px(TEXT_BASE))
        .text_color(rgb(TEXT))
        .child(text.into())
}
