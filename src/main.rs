mod ai;

use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

struct Warren {
    status: SharedString,
}

impl Warren {
    fn ask_claude(&mut self, cx: &mut Context<Self>) {
        self.status = "Asking Claude…".into();
        cx.notify();

        let reply = ai::spawn_hello();
        cx.spawn(async move |this, cx| {
            let status = match reply.await {
                Ok(Ok(text)) => text,
                Ok(Err(err)) => format!("Error: {err:#}"),
                Err(err) => format!("Error: {err}"),
            };
            this.update(cx, |this, cx| {
                this.status = status.into();
                cx.notify();
            })
        })
        .detach();
    }
}

impl Render for Warren {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .justify_center()
            .items_center()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(div().text_3xl().child("Hello, Warren"))
            .child(
                div()
                    .id("ask-claude")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0xb4befe)))
                    .child("Ask Claude")
                    .on_click(cx.listener(|this, _, _, cx| this.ask_claude(cx))),
            )
            .child(div().max_w(px(600.)).child(self.status.clone()))
    }
}

fn main() {
    dotenvy::dotenv().ok();

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Warren {
                    status: "Click the button to say hi to Claude.".into(),
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
