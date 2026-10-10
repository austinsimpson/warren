mod agent_editor;
mod agents;
mod agents_ui;
mod autopilot_ui;
mod backtest;
mod chart;
mod config;
mod market;
mod portfolio;
mod replay;
mod runtime;
mod scorecard;
mod scorecard_ui;
mod sectors;
mod store;
mod strategy;
mod text_input;
mod theme;

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use gpui::{
    App, Bounds, Context, Div, ElementId, Entity, FontWeight, SharedString, Stateful, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, point, prelude::*, px, rgb, size,
    transparent_black,
};

use autopilot_ui::AgentsState;
use chart::ChartRange;
use market::alpaca::{AlpacaClient, Bar, Snapshot};
use portfolio::Portfolio;
use theme::*;

/// Always shown at the top of the watchlist for market context.
const MARKET_SYMBOLS: &[&str] = &["SPY", "QQQ"];
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
/// The header doubles as the (transparent) title bar.
const HEADER_HEIGHT: f32 = 38.;
/// Room for the macOS traffic lights at the left of the header.
const TRAFFIC_LIGHT_INSET: f32 = 80.;

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
    portfolio: Portfolio,
    /// One runtime per user-created agent (strategy).
    agents: Vec<AgentsState>,
    /// Index into `agents` of the agent the UI shows.
    selected_agent: usize,
    agents_error: Option<SharedString>,
    /// For creating and editing agents.
    home: Option<autopilot_ui::AgentHome>,
    /// The open agent form, which replaces the chart while open, its
    /// subscription, and whether it creates a new agent.
    editor: Option<(Entity<agent_editor::AgentEditor>, Subscription, bool)>,
    bottom_tab: scorecard_ui::BottomTab,
    scorecard: scorecard_ui::ScorecardState,
}

impl Warren {
    fn new(cx: &mut Context<Self>) -> Self {
        let (alpaca, market_status) = match AlpacaClient::from_env() {
            Ok(client) => (Some(Arc::new(client)), "Loading…".into()),
            Err(err) => (None, format!("Market data unavailable: {err:#}").into()),
        };
        let (agents, home, agents_error) = match alpaca.clone().map(|a| autopilot_ui::start(a, cx))
        {
            Some(Ok((agents, home))) => (agents, Some(home), None),
            Some(Err(err)) => (
                Vec::new(),
                None,
                Some(format!("Autopilot unavailable: {err:#}").into()),
            ),
            None => (
                Vec::new(),
                None,
                Some("Autopilot needs Alpaca credentials".into()),
            ),
        };
        let mut this = Self {
            alpaca,
            selected: MARKET_SYMBOLS[0].to_string(),
            watchlist: Vec::new(),
            snapshots: HashMap::new(),
            market_status,
            range: ChartRange::Day,
            bars: Vec::new(),
            chart_status: "".into(),
            chart_generation: 0,
            portfolio: Portfolio::default(),
            agents,
            selected_agent: 0,
            agents_error,
            home,
            editor: None,
            bottom_tab: Default::default(),
            scorecard: Default::default(),
        };
        this.refresh_quotes(cx);
        this.load_chart(cx);
        this.refresh_portfolio(cx);

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let alive = this.update(cx, |this, cx| {
                    this.refresh_quotes(cx);
                    this.refresh_portfolio(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        this
    }

    fn refresh_quotes(&mut self, cx: &mut Context<Self>) {
        let Some(alpaca) = self.alpaca.clone() else {
            return;
        };
        self.watchlist = self
            .watchlist_sections()
            .into_iter()
            .flat_map(|(_, symbols)| symbols)
            .collect();
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
            self.clear_pending_order();
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

    /// Market context, current holdings, then exploration candidates, each
    /// symbol listed once.
    fn watchlist_sections(&self) -> Vec<(&'static str, Vec<String>)> {
        let mut seen = std::collections::HashSet::new();
        let mut section = |symbols: Vec<String>| -> Vec<String> {
            symbols
                .into_iter()
                .filter(|s| seen.insert(s.clone()))
                .collect()
        };
        vec![
            (
                "Market",
                section(MARKET_SYMBOLS.iter().map(|s| s.to_string()).collect()),
            ),
            ("Holdings", section(self.held_symbols())),
            ("Candidates", section(self.candidate_symbols())),
        ]
    }

    fn render_watchlist(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let sections: Vec<Div> = self
            .watchlist_sections()
            .into_iter()
            .filter(|(_, symbols)| !symbols.is_empty())
            .map(|(name, symbols)| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().px_2().pt_2().pb_1().child(label(name)))
                    .children(symbols.iter().map(|symbol| self.render_row(symbol, cx)))
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .w(px(200.))
            .flex_none()
            .p_2()
            .gap_1()
            .border_r_1()
            .border_color(rgb(BORDER))
            .id("watchlist")
            .overflow_y_scroll()
            .child(self.render_agent_list(cx))
            .children(sections)
            .when(!self.market_status.is_empty(), |col| {
                col.child(
                    div()
                        .px_2()
                        .text_size(px(TEXT_XS))
                        .text_color(rgb(WARNING))
                        .child(self.market_status.clone()),
                )
            })
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
            .items_center()
            .px_2()
            .py_1()
            .border_l_2()
            .border_color(transparent_black())
            .cursor_pointer()
            .when(selected, |row| {
                row.bg(rgb(SELECTED)).border_color(rgb(ACCENT))
            })
            .when(!selected, |row| row.hover(|style| style.bg(rgb(HOVER))))
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::MEDIUM)
                    .child(symbol.clone()),
            )
            .child(div().w(px(64.)).flex().justify_end().child(price))
            .child(
                div()
                    .w(px(60.))
                    .flex()
                    .justify_end()
                    .text_size(px(TEXT_SM))
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
            Some(c) if c < 0.0 => RED,
            _ => GREEN,
        };

        let header = div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .text_size(px(TEXT_LG))
                    .font_weight(FontWeight::BOLD)
                    .child(self.selected.clone()),
            )
            .child(last.map_or("".into(), |bar| format!("{:.2}", bar.close)))
            .child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(change_color)
                    .child(change_text),
            );

        let range_buttons =
            div()
                .flex()
                .flex_row()
                .gap_0p5()
                .children(ChartRange::ALL.into_iter().map(|range| {
                    ghost_button(range.label(), range.label(), range == self.range)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_range(range, cx)))
                }));

        let (high, low) = self
            .bars
            .iter()
            .fold((f64::MIN, f64::MAX), |(hi, lo), bar| {
                (hi.max(bar.high), lo.min(bar.low))
            });

        let axis_text = || {
            div()
                .flex()
                .flex_row()
                .text_size(px(TEXT_XS))
                .text_color(rgb(MUTED))
        };

        let body: Div = if self.chart_status.is_empty() {
            let closes = self.bars.iter().map(|bar| bar.close).collect();
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap_1()
                .child(
                    axis_text()
                        .gap_3()
                        .child(format!("H {high:.2}"))
                        .child(format!("L {low:.2}")),
                )
                .child(div().flex_1().child(chart::price_chart(
                    closes,
                    line_color,
                    self.research_levels(),
                )))
                .child(
                    axis_text()
                        .justify_between()
                        .children(first.map(|bar| self.range.format_time(bar.timestamp)))
                        .children(last.map(|bar| self.range.format_time(bar.timestamp))),
                )
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(TEXT_SM))
                .text_color(rgb(MUTED))
                .child(self.chart_status.clone())
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(header)
                    .child(range_buttons),
            )
            .children(self.render_research_card())
            .child(body)
    }
}

impl Render for Warren {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .h(px(HEADER_HEIGHT))
            .pl(px(TRAFFIC_LIGHT_INSET))
            .pr_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_size(px(TEXT_BASE))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(ACCENT))
                    .child("WARREN"),
            )
            .child(self.render_account_summary())
            .child(div().flex_1())
            .child(self.render_autopilot_controls(cx))
            .child(
                ghost_button("refresh", "Refresh", false).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.refresh_quotes(cx);
                        this.refresh_portfolio(cx);
                    },
                )),
            );

        let center = match &self.editor {
            Some((editor, _, _)) => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(editor.clone()),
            None => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(self.render_chart_panel(cx))
                .child(self.render_order_ticket(cx)),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family(MONO)
            .text_size(px(TEXT_BASE))
            .font_features(tabular_numbers())
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_watchlist(cx))
                    .child(center)
                    .child(self.render_positions_and_orders(cx)),
            )
            .child(self.render_bottom_panel(cx))
    }
}

fn main() {
    dotenvy::dotenv().ok();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some("backtest") => Some(backtest::run_cli(&args[1..])),
        Some("scorecard") => Some(scorecard::run_cli(&args[1..])),
        Some("replay") => Some(replay::run_cli(&args[1..])),
        _ => None,
    };
    if let Some(result) = command {
        if let Err(err) = result {
            eprintln!("{} failed: {err:#}", args[0]);
            std::process::exit(1);
        }
        return;
    }

    gpui_platform::application().run(|cx: &mut App| {
        if let Err(err) = theme::load_fonts(cx) {
            eprintln!("failed to load bundled fonts: {err:#}");
        }
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Warren".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px((HEADER_HEIGHT - 14.) / 2.))),
                }),
                ..Default::default()
            },
            |_, cx| cx.new(Warren::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
