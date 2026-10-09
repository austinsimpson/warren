mod ai;
mod market;
mod runtime;

use std::{collections::HashMap, sync::Arc};

use gpui::{
    App, Bounds, Context, Div, ElementId, SharedString, Stateful, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

use market::alpaca::{AlpacaClient, Snapshot};

const DEFAULT_WATCHLIST: &[&str] = &["SPY", "QQQ", "AAPL", "MSFT", "NVDA"];

struct Warren {
    alpaca: Option<Arc<AlpacaClient>>,
    watchlist: Vec<String>,
    snapshots: HashMap<String, Snapshot>,
    market_status: SharedString,
    claude_status: SharedString,
}

impl Warren {
    fn new(cx: &mut Context<Self>) -> Self {
        let (alpaca, market_status) = match AlpacaClient::from_env() {
            Ok(client) => (Some(Arc::new(client)), "Loading…".into()),
            Err(err) => (None, format!("Market data unavailable: {err:#}").into()),
        };
        let mut this = Self {
            alpaca,
            watchlist: DEFAULT_WATCHLIST.iter().map(|s| s.to_string()).collect(),
            snapshots: HashMap::new(),
            market_status,
            claude_status: "".into(),
        };
        this.refresh_quotes(cx);
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

    fn render_row(&self, symbol: &str) -> Div {
        let snapshot = self.snapshots.get(symbol);
        let price = snapshot
            .and_then(Snapshot::last_price)
            .map_or("—".into(), |p| format!("{p:.2}"));
        let change = snapshot.and_then(Snapshot::change_percent);
        let (change_text, change_color) = match change {
            Some(c) if c >= 0.0 => (format!("+{c:.2}%"), rgb(0xa6e3a1)),
            Some(c) => (format!("{c:.2}%"), rgb(0xf38ba8)),
            None => ("—".into(), rgb(0x6c7086)),
        };

        div()
            .flex()
            .flex_row()
            .py_1()
            .child(div().w(px(100.)).child(symbol.to_string()))
            .child(div().w(px(100.)).flex().justify_end().child(price))
            .child(
                div()
                    .w(px(100.))
                    .flex()
                    .justify_end()
                    .text_color(change_color)
                    .child(change_text),
            )
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
                    .flex_col()
                    .children(self.watchlist.iter().map(|symbol| self.render_row(symbol))),
            )
            .child(
                div()
                    .text_color(rgb(0xf9e2af))
                    .child(self.market_status.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .mt_auto()
                    .child(
                        button("ask-claude", "Ask Claude")
                            .on_click(cx.listener(|this, _, _, cx| this.ask_claude(cx))),
                    )
                    .child(div().max_w(px(600.)).child(self.claude_status.clone())),
            )
    }
}

fn main() {
    dotenvy::dotenv().ok();

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
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
