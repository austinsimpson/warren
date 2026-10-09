mod ai;
mod chart;
mod market;
mod runtime;

use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use gpui::{
    App, Bounds, Context, Div, ElementId, Rgba, SharedString, Stateful, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

use chart::ChartRange;
use market::alpaca::{AlpacaClient, Bar, Snapshot};

const DEFAULT_WATCHLIST: &[&str] = &["SPY", "QQQ", "AAPL", "MSFT", "NVDA"];

const GREEN: u32 = 0xa6e3a1;
const RED: u32 = 0xf38ba8;
const MUTED: u32 = 0x6c7086;
const WARNING: u32 = 0xf9e2af;

struct Warren {
    alpaca: Option<Arc<AlpacaClient>>,
    watchlist: Vec<String>,
    snapshots: HashMap<String, Snapshot>,
    market_status: SharedString,
    selected: String,
    range: ChartRange,
    bars: Vec<Bar>,
    chart_status: SharedString,
    /// Bumped per chart request so a slow, superseded response is dropped.
    chart_generation: u64,
    claude_status: SharedString,
}

impl Warren {
    fn new(cx: &mut Context<Self>) -> Self {
        let (alpaca, market_status) = match AlpacaClient::from_env() {
            Ok(client) => (Some(Arc::new(client)), "Loading…".into()),
            Err(err) => (None, format!("Market data unavailable: {err:#}").into()),
        };
        let watchlist: Vec<String> = DEFAULT_WATCHLIST.iter().map(|s| s.to_string()).collect();
        let mut this = Self {
            alpaca,
            selected: watchlist[0].clone(),
            watchlist,
            snapshots: HashMap::new(),
            market_status,
            range: ChartRange::Day,
            bars: Vec::new(),
            chart_status: "".into(),
            chart_generation: 0,
            claude_status: "".into(),
        };
        this.refresh_quotes(cx);
        this.load_chart(cx);
        this
    }

    fn refresh_quotes(&mut self, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        let symbols = self.watchlist.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move { alpaca.snapshots(&symbols).await }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(snapshots) => {
                        this.snapshots = snapshots;
                        this.market_status = "".into();
                    }
                    Err(err) => this.market_status = format!("Error: {err:#}").into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn select(&mut self, symbol: String, cx: &mut Context<Self>) {
        if symbol != self.selected {
            self.selected = symbol;
            self.load_chart(cx);
        }
    }

    fn set_range(&mut self, range: ChartRange, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            self.load_chart(cx);
        }
    }

    fn load_chart(&mut self, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        self.chart_generation += 1;
        let generation = self.chart_generation;
        self.bars.clear();
        self.chart_status = "Loading…".into();
        cx.notify();

        let symbol = self.selected.clone();
        let range = self.range;
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let start = range.start(Utc::now());
                alpaca.bars(&symbol, range.timeframe(), start).await
            })
            .await;
            this.update(cx, |this, cx| {
                if this.chart_generation != generation {
                    return;
                }
                match result {
                    Ok(bars) => {
                        this.bars = range.trim(bars);
                        this.chart_status = if this.bars.len() < 2 {
                            "No data for this range.".into()
                        } else {
                            "".into()
                        };
                    }
                    Err(err) => this.chart_status = format!("Error: {err:#}").into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn ask_claude(&mut self, cx: &mut Context<Self>) {
        self.claude_status = "Asking Claude…".into();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let status = runtime::run(ai::hello())
                .await
                .unwrap_or_else(|err| format!("Error: {err:#}"));
            this.update(cx, |this, cx| {
                this.claude_status = status.into();
                cx.notify();
            })
        })
        .detach();
    }

    fn render_row(&self, symbol: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let snapshot = self.snapshots.get(symbol);
        let price = snapshot
            .and_then(Snapshot::last_price)
            .map_or("—".into(), |p| format!("{p:.2}"));
        let (change_text, change_color) =
            format_change(snapshot.and_then(Snapshot::change_percent));
        let selected = symbol == self.selected;
        let symbol = symbol.to_string();

        div()
            .id(ElementId::Name(symbol.clone().into()))
            .flex()
            .flex_row()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |row| row.bg(rgb(0x313244)))
            .when(!selected, |row| row.hover(|style| style.bg(rgb(0x181825))))
            .child(div().w(px(70.)).child(symbol.clone()))
            .child(div().w(px(90.)).flex().justify_end().child(price))
            .child(
                div()
                    .w(px(90.))
                    .flex()
                    .justify_end()
                    .text_color(change_color)
                    .child(change_text),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.select(symbol.clone(), cx)))
    }

    fn render_chart_panel(&self, cx: &mut Context<Self>) -> Div {
        let first = self.bars.first();
        let last = self.bars.last();
        let range_change = first
            .zip(last)
            .map(|(first, last)| (last.close - first.close) / first.close * 100.0);
        let (change_text, change_color) = format_change(range_change);
        let line_color = match range_change {
            Some(c) if c < 0.0 => rgb(RED),
            _ => rgb(GREEN),
        };

        let header = div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_3()
            .child(div().text_2xl().child(self.selected.clone()))
            .child(last.map_or("".into(), |bar| format!("{:.2}", bar.close)))
            .child(div().text_color(change_color).child(change_text));

        let range_buttons =
            div()
                .flex()
                .flex_row()
                .gap_1()
                .children(ChartRange::ALL.into_iter().map(|range| {
                    let active = range == self.range;
                    div()
                        .id(range.label())
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_sm()
                        .when(active, |b| b.bg(rgb(0x89b4fa)).text_color(rgb(0x1e1e2e)))
                        .when(!active, |b| b.hover(|style| style.bg(rgb(0x313244))))
                        .child(range.label())
                        .on_click(cx.listener(move |this, _, _, cx| this.set_range(range, cx)))
                }));

        let (high, low) = self
            .bars
            .iter()
            .fold((f64::MIN, f64::MAX), |(hi, lo), bar| {
                (hi.max(bar.high), lo.min(bar.low))
            });

        let body: Div = if self.chart_status.is_empty() {
            let closes = self.bars.iter().map(|bar| bar.close).collect();
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_4()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(format!("High {high:.2}"))
                        .child(format!("Low {low:.2}")),
                )
                .child(div().flex_1().child(chart::price_chart(closes, line_color)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .children(first.map(|bar| self.range.format_time(bar.timestamp)))
                        .children(last.map(|bar| self.range.format_time(bar.timestamp))),
                )
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(MUTED))
                .child(self.chart_status.clone())
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(header)
                    .child(range_buttons),
            )
            .child(body)
    }
}

fn format_change(change: Option<f64>) -> (String, Rgba) {
    match change {
        Some(c) if c >= 0.0 => (format!("+{c:.2}%"), rgb(GREEN)),
        Some(c) => (format!("{c:.2}%"), rgb(RED)),
        None => ("—".into(), rgb(MUTED)),
    }
}

fn button(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(0x89b4fa))
        .text_color(rgb(0x1e1e2e))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0xb4befe)))
        .child(label)
}

impl Render for Warren {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .watchlist
            .iter()
            .map(|symbol| self.render_row(symbol, cx))
            .collect();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .p_8()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(div().text_3xl().child("Warren"))
                    .child(
                        button("refresh", "Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_quotes(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .gap_8()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().flex().flex_col().children(rows))
                            .child(
                                div()
                                    .max_w(px(250.))
                                    .text_color(rgb(WARNING))
                                    .child(self.market_status.clone()),
                            ),
                    )
                    .child(self.render_chart_panel(cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        button("ask-claude", "Ask Claude")
                            .on_click(cx.listener(|this, _, _, cx| this.ask_claude(cx))),
                    )
                    .child(div().flex_1().child(self.claude_status.clone())),
            )
    }
}

fn main() {
    dotenvy::dotenv().ok();

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(Warren::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
