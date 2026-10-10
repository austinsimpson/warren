//! The bottom panel: the activity feed, the scorecard, and usage, as tabs.

use chrono::{Duration, Local, Utc};
use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*, px, rgb};

use crate::scorecard::{self, FactorStudy, MIN_SAMPLE, Scorecard, UsageTotals, tokens};
use crate::theme::*;
use crate::{Warren, runtime};

/// A scorecard older than this is recomputed when its tab is opened.
const SCORECARD_STALE_AFTER: Duration = Duration::minutes(10);
const PANEL_HEIGHT: f32 = 150.;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BottomTab {
    #[default]
    Activity,
    Scorecard,
    Usage,
}

impl BottomTab {
    const ALL: [Self; 3] = [Self::Activity, Self::Scorecard, Self::Usage];

    fn title(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Scorecard => "Scorecard",
            Self::Usage => "Usage",
        }
    }
}

/// The scorecard and its loading state.
#[derive(Default)]
pub struct ScorecardState {
    card: Option<Scorecard>,
    loading: bool,
    error: Option<SharedString>,
}

impl Warren {
    fn set_bottom_tab(&mut self, tab: BottomTab, cx: &mut Context<Self>) {
        self.bottom_tab = tab;
        let stale = self
            .scorecard
            .card
            .as_ref()
            .is_none_or(|c| Utc::now() - c.computed_at > SCORECARD_STALE_AFTER);
        if tab == BottomTab::Scorecard && stale {
            self.refresh_scorecard(cx);
        }
        cx.notify();
    }

    fn refresh_scorecard(&mut self, cx: &mut Context<Self>) {
        let (Some(alpaca), Some(agents)) = (self.alpaca.clone(), self.agents.as_ref()) else {
            return;
        };
        if self.scorecard.loading {
            return;
        }
        self.scorecard.loading = true;
        let store = agents.store();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::run(async move { scorecard::compute(&alpaca, &store).await }).await;
            this.update(cx, |this, cx| {
                let state = &mut this.scorecard;
                state.loading = false;
                match result {
                    Ok(card) => {
                        state.card = Some(card);
                        state.error = None;
                    }
                    Err(err) => state.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn render_bottom_panel(&self, cx: &mut Context<Self>) -> Div {
        let tabs = BottomTab::ALL.map(|tab| {
            let active = tab == self.bottom_tab;
            div()
                .id(tab.title())
                .text_size(px(TEXT_XS))
                .font_weight(if active {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(rgb(if active { TEXT } else { MUTED }))
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(TEXT)))
                .child(tab.title().to_uppercase())
                .on_click(cx.listener(move |this, _, _, cx| this.set_bottom_tab(tab, cx)))
        });

        let status: Option<Div> = match self.bottom_tab {
            BottomTab::Activity => self.render_activity_status(),
            BottomTab::Scorecard => Some(self.render_scorecard_status(cx)),
            BottomTab::Usage => None,
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .children(tabs)
            .children(status);

        let body = match self.bottom_tab {
            BottomTab::Activity => self.render_activity().into_any_element(),
            BottomTab::Scorecard => self.render_scorecard().into_any_element(),
            BottomTab::Usage => self.render_usage().into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .h(px(PANEL_HEIGHT))
            .flex_none()
            .px_3()
            .py_2()
            .gap_1()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(header)
            .child(body)
    }

    fn render_scorecard_status(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.scorecard;
        let text = if state.loading {
            "updating…".to_string()
        } else if let Some(err) = &state.error {
            err.to_string()
        } else if let Some(card) = &state.card {
            format!(
                "updated {}",
                card.computed_at.with_timezone(&Local).format("%H:%M")
            )
        } else {
            String::new()
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(TEXT_XS))
                    .text_color(rgb(if state.error.is_some() { RED } else { MUTED }))
                    .child(text),
            )
            .child(
                ghost_button("scorecard-refresh", "Refresh", false)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_scorecard(cx))),
            )
    }

    fn render_scorecard(&self) -> impl IntoElement {
        let body = div()
            .id("scorecard")
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .gap_6()
            .overflow_y_scroll()
            .text_size(px(TEXT_SM));
        let Some(card) = &self.scorecard.card else {
            return body.child(label(if self.scorecard.loading {
                "Computing…"
            } else {
                "No scorecard yet"
            }));
        };

        let performance = match &card.performance {
            Some(p) => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(label(format!("since {}", p.since.format("%b %-d"))))
                .child(stat_row("Warren", p.warren))
                .child(stat_row("SPY", p.spy))
                .child(stat_row("difference", p.warren - p.spy)),
            None => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().text_color(rgb(MUTED)).child("No trades yet"))
                .child(label("measured from the first trade")),
        };
        let d = &card.decisions;
        let decisions = (d.filled_buys > 0).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(label(format!(
                    "{} filled buy{}, {} beat SPY",
                    d.filled_buys,
                    if d.filled_buys == 1 { "" } else { "s" },
                    d.beat_spy
                )))
                .children(d.mean_excess.map(|x| stat_row("avg vs SPY", x)))
        });

        body.child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .w(px(190.))
                .gap_2()
                .child(label("performance"))
                .child(performance)
                .children(decisions),
        )
        .child(factor_table(&card.factors))
    }

    fn render_usage(&self) -> impl IntoElement {
        let Some(agents) = &self.agents else {
            return div().child(label("Usage unavailable")).into_any_element();
        };
        let usage = &agents.usage;
        let mut rows = vec![
            usage_row("today", &usage.today, true),
            usage_row("7 days", &usage.week, true),
            usage_row("all time", &usage.all, true),
        ];
        for (kind, totals) in &usage.by_agent {
            rows.push(usage_row(&format!("  {kind}"), totals, false));
        }
        let header = [
            "", "runs", "input", "cached", "output", "jev", "cost", "per run",
        ];
        div()
            .id("usage")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .text_size(px(TEXT_SM))
            .child(table_row(header.into_iter().map(label).collect()))
            .children(rows)
            .children((usage.all.estimated_runs > 0).then(|| {
                label(format!(
                    "{} older runs priced from token counts, assuming no prompt caching",
                    usage.all.estimated_runs
                ))
                .mt_1()
            }))
            .into_any_element()
    }
}

/// Fixed column widths for usage rows.
const USAGE_COLUMNS: [f32; 8] = [90., 50., 70., 70., 70., 60., 70., 70.];

fn table_row(cells: Vec<Div>) -> Div {
    div()
        .flex()
        .flex_row()
        .children(
            cells
                .into_iter()
                .zip(USAGE_COLUMNS)
                .enumerate()
                .map(|(i, (cell, width))| {
                    let cell = cell.w(px(width)).flex_none();
                    if i == 0 {
                        cell
                    } else {
                        cell.flex().justify_end()
                    }
                }),
        )
}

fn usage_row(name: &str, t: &UsageTotals, strong: bool) -> Div {
    let color = if strong { TEXT } else { MUTED };
    let cell = |text: String| div().text_color(rgb(color)).child(text);
    table_row(vec![
        div().text_color(rgb(color)).child(name.to_string()),
        cell(t.runs.to_string()),
        cell(tokens(t.input_tokens)),
        cell(tokens(t.cached_input_tokens)),
        cell(tokens(t.output_tokens)),
        cell(tokens(t.jev_input_tokens)),
        cell(format!("${:.2}", t.cost_usd)),
        cell(t.cost_per_run().map_or("–".into(), |c| format!("${c:.3}"))),
    ])
}

fn stat_row(name: &str, value: f64) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .child(div().text_color(rgb(MUTED)).child(name.to_string()))
        .child(
            div()
                .text_color(rgb(signed_color(value)))
                .child(format!("{:+.2}%", value * 100.0)),
        )
}

fn signed_color(value: f64) -> u32 {
    if value > 0.0 {
        GREEN
    } else if value < 0.0 {
        RED
    } else {
        MUTED
    }
}

/// Each factor's rank correlation with returns above SPY per horizon, faded
/// until enough reports have matured to mean anything.
fn factor_table(study: &FactorStudy) -> Div {
    const NAME_WIDTH: f32 = 100.;
    const CELL_WIDTH: f32 = 120.;
    let mut note = format!(
        "{} report{} · IC = rank correlation with return vs SPY",
        study.reports,
        if study.reports == 1 { "" } else { "s" }
    );
    if let Some(due) = study.next_maturity {
        note += &format!(" · next matures ~{}", due.format("%b %-d"));
    }
    let header = div()
        .flex()
        .flex_row()
        .child(div().w(px(NAME_WIDTH)).flex_none().child(label("factor")))
        .children(study.horizons.iter().map(|h| {
            div()
                .w(px(CELL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .child(label(format!("{}d · n {}", h.days, h.matured)))
        }));
    let names = study
        .horizons
        .first()
        .map(|h| h.factors.iter().map(|f| f.name).collect::<Vec<_>>())
        .unwrap_or_default();
    let rows = names.into_iter().enumerate().map(|(k, name)| {
        div()
            .flex()
            .flex_row()
            .child(
                div()
                    .w(px(NAME_WIDTH))
                    .flex_none()
                    .text_color(rgb(MUTED))
                    .child(name),
            )
            .children(study.horizons.iter().map(move |h| {
                let stat = &h.factors[k];
                let reliable = stat.n >= MIN_SAMPLE;
                let (text, color) = match stat.ic {
                    Some(ic) => {
                        let spread = stat
                            .spread
                            .map_or(String::new(), |s| format!("  {:+.1}%", s * 100.0));
                        (format!("{ic:+.2}{spread}"), signed_color(ic))
                    }
                    None => ("–".into(), MUTED),
                };
                div()
                    .w(px(CELL_WIDTH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_color(if reliable {
                        rgb(color).into()
                    } else {
                        tint(color, 0.45)
                    })
                    .child(text)
            }))
    });
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap_0p5()
        .child(label(note))
        .child(header)
        .children(rows)
}
