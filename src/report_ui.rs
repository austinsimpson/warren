//! The full research report for the selected symbol, shown in place of the
//! chart: everything the research agent wrote and Warren computed.

use chrono::Local;
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, div, prelude::*, px, rgb};

use crate::Warren;
use crate::agents::research::ResearchReport;
use crate::autopilot_ui::score_color;
use crate::theme::*;

impl Warren {
    pub fn open_report(&mut self, cx: &mut Context<Self>) {
        self.report_open = true;
        cx.notify();
    }

    fn close_report(&mut self, cx: &mut Context<Self>) {
        self.report_open = false;
        cx.notify();
    }

    /// The report for the selected symbol, if one is open and exists.
    pub fn render_report(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.report_open {
            return None;
        }
        let report = self.agent()?.reports.get(&self.selected)?;
        Some(report_view(report, cx).into_any_element())
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

/// A bulleted line of prose.
fn bullet(mark: &'static str, color: u32, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(div().flex_none().text_color(rgb(color)).child(mark))
        .child(prose(text).flex_1().min_w_0())
}

fn stat(name: &str, value: String) -> Div {
    div()
        .flex()
        .flex_row()
        .gap_1()
        .child(label(name.to_string()))
        .child(value)
}

fn report_view(report: &ResearchReport, cx: &mut Context<Warren>) -> impl IntoElement {
    let f = &report.findings;
    let money = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.2}"));
    let written = report
        .generated_at
        .with_timezone(&Local)
        .format("%a %b %-d, %-I:%M %p");

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .text_size(px(TEXT_LG))
                .font_weight(FontWeight::BOLD)
                .child(format!("{} research", report.symbol)),
        )
        .child(label(format!(
            "{written} · {} · at {}",
            report.model,
            money(report.price_at_research)
        )))
        .child(div().flex_1())
        .child(
            ghost_button("close-report", "Close", false)
                .on_click(cx.listener(|this, _, _, cx| this.close_report(cx))),
        );

    let levels = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_x_4()
        .gap_y_1()
        .text_size(px(TEXT_SM))
        .child(stat(
            "entry",
            match (f.entry_low, f.entry_high) {
                (Some(lo), Some(hi)) => format!("{lo:.2}–{hi:.2}"),
                _ => "no long setup".into(),
            },
        ))
        .child(stat("target", money(f.target_price)))
        .child(stat("stop", money(f.stop_loss)))
        .child(stat(
            "reward/risk",
            report
                .risk_reward
                .map_or("–".into(), |rr| format!("{rr:.2}:1")),
        ))
        .child(stat("horizon", format!("{} trading days", f.horizon_days)));

    // Every factor with the reason behind its score.
    let factors = report.factors.named().into_iter().map(|(name, factor)| {
        let shadow = report.shadow.as_ref().and_then(|s| match name {
            "catalyst" => Some(&s.catalyst),
            "news" => Some(&s.news_sentiment),
            _ => None,
        });
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                div()
                    .w(px(90.))
                    .flex_none()
                    .text_size(px(TEXT_SM))
                    .text_color(rgb(MUTED))
                    .child(name),
            )
            .child(
                div()
                    .w(px(16.))
                    .flex_none()
                    .text_color(rgb(score_color(factor.score)))
                    .child(factor.score.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(SANS)
                    .text_size(px(TEXT_SM))
                    .text_color(rgb(TEXT))
                    .child(factor.reason.clone()),
            )
            .children(shadow.map(|s| {
                label(format!("jev {:.1} ({:.0}%)", s.score, s.confidence * 100.0)).flex_none()
            }))
    });

    let fundamentals = report.fundamentals.map(|snapshot| snapshot.describe());
    let trend = report.trend_health.map(|h| {
        format!(
            "{:.0}% below the 52-week high{}",
            h.percent_below_52w_high,
            h.return_12_1_percent
                .map_or(String::new(), |r| format!("; 12-1 month return {r:+.0}%"))
        )
    });

    div()
        .id("report")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px_4()
        .py_3()
        .gap_1()
        .child(header)
        .child(section("levels"))
        .child(levels)
        .child(section("thesis"))
        .child(prose(f.thesis.clone()))
        .when(!f.catalysts.is_empty(), |d| {
            d.child(section("catalysts"))
                .children(f.catalysts.iter().map(|c| bullet("•", ACCENT, c.clone())))
        })
        .child(section("risks"))
        .children(f.risks.iter().map(|r| bullet("▲", RED, r.clone())))
        .when(!f.news_highlights.is_empty(), |d| {
            d.child(section("news highlights"))
                .children(f.news_highlights.iter().map(|n| {
                    div()
                        .flex()
                        .flex_col()
                        .pb_1()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(label(n.date.clone()).flex_none())
                                .child(prose(n.headline.clone()).font_weight(FontWeight::MEDIUM)),
                        )
                        .child(
                            prose(n.significance.clone())
                                .text_size(px(TEXT_SM))
                                .text_color(rgb(MUTED)),
                        )
                }))
        })
        .child(section("factors"))
        .children(factors)
        .children(trend.map(|t| prose(t).text_size(px(TEXT_SM)).text_color(rgb(MUTED))))
        .children(fundamentals.map(|text| {
            div()
                .flex()
                .flex_col()
                .child(section("fundamentals (sec filings)"))
                .child(prose(text).text_size(px(TEXT_SM)))
        }))
        .child(section("technicals"))
        .child(prose(f.technical_summary.clone()).text_size(px(TEXT_SM)))
        .child(section("news"))
        .child(prose(f.news_summary.clone()).text_size(px(TEXT_SM)))
        .child(div().h(px(24.)))
}
