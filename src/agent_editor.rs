//! The form for creating or editing an agent (a strategy).

use gpui::{
    Context, Div, Entity, EventEmitter, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px, rgb,
};

use crate::strategy::{Horizon, LimitOverrides, RiskTolerance, Sector, Strategy, slug};
use crate::text_input::TextInput;
use crate::theme::*;

pub enum EditorEvent {
    Saved(Strategy),
    Cancelled,
}

/// Models offered for every stage.
const MODELS: [(&str, &str); 3] = [
    ("claude-opus-5-5", "Opus 5.5"),
    ("claude-sonnet-5-5", "Sonnet 5.5"),
    ("claude-haiku-5-5", "Haiku 5.5"),
];

pub struct AgentEditor {
    draft: Strategy,
    is_new: bool,
    /// IDs already taken by other agents.
    taken_ids: Vec<String>,
    /// Budget the other agents already claim, and the account's equity, to
    /// keep budgets within the account.
    other_budgets: f64,
    account_equity: Option<f64>,
    name: Entity<TextInput>,
    budget: Entity<TextInput>,
    excluded: Entity<TextInput>,
    instructions: Entity<TextInput>,
    error: Option<SharedString>,
}

impl EventEmitter<EditorEvent> for AgentEditor {}

impl AgentEditor {
    pub fn new(
        draft: Strategy,
        is_new: bool,
        taken_ids: Vec<String>,
        other_budgets: f64,
        account_equity: Option<f64>,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "Agent name", &draft.name));
        let budget = cx.new(|cx| TextInput::new(cx, "25000", &format!("{:.0}", draft.budget)));
        let excluded = cx.new(|cx| {
            TextInput::new(
                cx,
                "e.g. TSLA, GME",
                &draft.universe.excluded_symbols.join(", "),
            )
        });
        let instructions = cx.new(|cx| {
            TextInput::new(
                cx,
                "e.g. Focus on post-earnings drift in mid-cap software; avoid biotech \
                 binary events.",
                &draft.instructions,
            )
            .multiline(4)
        });
        Self {
            draft,
            is_new,
            taken_ids,
            other_budgets,
            account_equity,
            name,
            budget,
            excluded,
            instructions,
            error: None,
        }
    }

    fn update(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Strategy)) {
        f(&mut self.draft);
        self.draft.overrides = std::mem::take(&mut self.draft.overrides)
            .without_preset_values(self.draft.risk.limits());
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.collect(cx) {
            Ok(strategy) => cx.emit(EditorEvent::Saved(strategy)),
            Err(err) => {
                self.error = Some(err.into());
                cx.notify();
            }
        }
    }

    /// The draft with the text fields applied, checked.
    fn collect(&self, cx: &Context<Self>) -> Result<Strategy, String> {
        let mut s = self.draft.clone();
        s.name = self.name.read(cx).text().trim().to_string();
        if s.name.is_empty() {
            return Err("Give the agent a name.".into());
        }
        let budget_text = self.budget.read(cx).text().replace([',', '$', ' '], "");
        s.budget = budget_text
            .parse()
            .map_err(|_| format!("Budget {budget_text:?} isn't a number."))?;
        if s.budget <= 0.0 {
            return Err("The budget must be positive.".into());
        }
        if let Some(equity) = self.account_equity
            && self.other_budgets + s.budget > equity
        {
            return Err(format!(
                "Budgets would total ${:.0}, more than the account's ${equity:.0}. At most \
                 ${:.0} is unallocated.",
                self.other_budgets + s.budget,
                (equity - self.other_budgets).max(0.0)
            ));
        }
        s.universe.excluded_symbols = self
            .excluded
            .read(cx)
            .text()
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_uppercase())
            .collect();
        s.instructions = self.instructions.read(cx).text().trim().to_string();
        if self.is_new {
            let base = slug(&s.name);
            if base.is_empty() {
                return Err("The name needs at least one letter or digit.".into());
            }
            let mut id = base.clone();
            let mut n = 2;
            while self.taken_ids.contains(&id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            s.id = id;
        }
        s.validate().map_err(|e| format!("{e:#}"))?;
        Ok(s)
    }
}

/// A labeled form row.
fn row(name: &str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(150.))
                .flex_none()
                .text_color(rgb(MUTED))
                .child(name.to_string()),
        )
        .child(div().flex_1().min_w_0().child(control))
}

fn section(title: &str) -> Div {
    div().pt_3().pb_1().child(label(title.to_string()))
}

/// A row of mutually exclusive choices.
fn segmented(choices: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_1()
        .children(choices)
}

impl Render for AgentEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let d = &self.draft;
        let limits = d.limits();

        // Each limit: a value with − and + that set an override.
        macro_rules! limit {
            ($id:literal, $name:literal, $field:ident, $step:expr, $min:expr, $max:expr, $fmt:expr) => {{
                let value = limits.$field;
                let overridden = d.overrides.$field.is_some();
                let fmt: fn(f64) -> String = $fmt;
                row(
                    $name,
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(ghost_button(concat!($id, "-down"), "−", false).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.update(cx, |s| {
                                    let v = (value as f64 - $step as f64).max($min as f64);
                                    s.overrides.$field = Some(v as _);
                                })
                            }),
                        ))
                        .child(
                            div()
                                .w(px(64.))
                                .flex()
                                .justify_center()
                                .text_color(if overridden { rgb(ACCENT) } else { rgb(TEXT) })
                                .child(fmt(value as f64)),
                        )
                        .child(
                            ghost_button(concat!($id, "-up"), "+", false).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.update(cx, |s| {
                                        let v = (value as f64 + $step as f64).min($max as f64);
                                        s.overrides.$field = Some(v as _);
                                    })
                                },
                            )),
                        )
                        .when(overridden, |r| {
                            r.child(
                                ghost_button(concat!($id, "-reset"), "reset", false).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.update(cx, |s| s.overrides.$field = None)
                                    }),
                                ),
                            )
                        }),
                )
            }};
        }

        // A schedule or universe number with − and +.
        macro_rules! stepper {
            ($id:literal, $name:literal, $($path:ident).+, $step:expr, $min:expr, $max:expr, $fmt:expr) => {{
                let value = d.$($path).+;
                let fmt: fn(f64) -> String = $fmt;
                row(
                    $name,
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(ghost_button(concat!($id, "-down"), "−", false).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.update(cx, |s| {
                                    s.$($path).+ = ((value as f64 - $step as f64).max($min as f64)) as _
                                })
                            }),
                        ))
                        .child(
                            div()
                                .w(px(64.))
                                .flex()
                                .justify_center()
                                .child(fmt(value as f64)),
                        )
                        .child(ghost_button(concat!($id, "-up"), "+", false).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.update(cx, |s| {
                                    s.$($path).+ = ((value as f64 + $step as f64).min($max as f64)) as _
                                })
                            }),
                        )),
                )
            }};
        }

        let risk_choices = RiskTolerance::ALL
            .iter()
            .map(|&risk| {
                ghost_button(risk.label(), risk.label(), d.risk == risk).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.update(cx, |s| {
                            s.risk = risk;
                            s.overrides = LimitOverrides::default();
                        })
                    },
                ))
            })
            .collect();
        let horizon_choices = Horizon::ALL
            .iter()
            .map(|&h| {
                ghost_button(
                    h.label(),
                    format!("{} ({})", h.label(), h.describe()),
                    d.horizon == h,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.update(cx, |s| s.horizon = h)))
            })
            .collect();
        let all_sectors = d.universe.sectors.is_empty();
        let mut sector_choices = vec![ghost_button("all-sectors", "All", all_sectors).on_click(
            cx.listener(|this, _, _, cx| this.update(cx, |s| s.universe.sectors.clear())),
        )];
        sector_choices.extend(Sector::ALL.iter().map(|&sector| {
            let on = d.universe.sectors.contains(&sector);
            ghost_button(sector.label(), sector.label(), on).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.update(cx, |s| {
                        let sectors = &mut s.universe.sectors;
                        if let Some(i) = sectors.iter().position(|x| *x == sector) {
                            sectors.remove(i);
                        } else {
                            sectors.push(sector);
                            sectors.sort();
                        }
                    })
                },
            ))
        }));
        let current_model = d.models.research.model.clone();
        let model_choices = MODELS
            .iter()
            .map(|&(id, name)| {
                ghost_button(id, name, current_model == id).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.update(cx, |s| {
                            for stage in [
                                &mut s.models.exploration,
                                &mut s.models.research,
                                &mut s.models.executor,
                            ] {
                                stage.model = id.to_string();
                            }
                        })
                    },
                ))
            })
            .collect();
        let toggle = |id: &'static str, on: bool, text: &'static str| {
            ghost_button(
                id,
                if on {
                    format!("✓ {text}")
                } else {
                    format!("○ {text}")
                },
                on,
            )
        };
        let unallocated = self.account_equity.map(|e| {
            format!(
                "${:.0} of the account is unallocated",
                (e - self.other_budgets).max(0.0)
            )
        });
        let preset_note = format!(
            "{} preset; accent values override it. Max order is a percent of the budget.",
            d.risk.label()
        );

        div()
            .id("agent-editor")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .gap_1p5()
            .text_size(px(TEXT_SM))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(TEXT_LG))
                            .font_weight(FontWeight::BOLD)
                            .child(if self.is_new {
                                "New agent".to_string()
                            } else {
                                format!("Edit {}", d.name)
                            }),
                    )
                    .child(
                        ghost_button("editor-cancel", "Cancel", false)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::Cancelled))),
                    )
                    .child(
                        button("editor-save", "Save", ACCENT)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_color(rgb(RED)).child(e)),
            )
            .child(section("basics"))
            .child(row("Name", self.name.clone()))
            .child(row("Budget ($)", self.budget.clone()))
            .children(unallocated.map(|u| row("", label(u))))
            .child(row("Model", segmented(model_choices)))
            .child(row(
                "Start",
                toggle("autostart", d.autostart, "run automatically at launch").on_click(
                    cx.listener(|this, _, _, cx| this.update(cx, |s| s.autostart = !s.autostart)),
                ),
            ))
            .child(section("risk"))
            .child(row("Tolerance", segmented(risk_choices)))
            .child(row("", label(preset_note)))
            .child(limit!(
                "pos",
                "Max position",
                max_position_percent,
                1.0,
                1.0,
                100.0,
                |v| format!("{v:.0}%")
            ))
            .child(limit!(
                "ord",
                "Max order",
                max_order_percent,
                1.0,
                1.0,
                100.0,
                |v| format!("{v:.0}%")
            ))
            .child(limit!(
                "cash",
                "Cash reserve",
                min_cash_percent,
                5.0,
                0.0,
                90.0,
                |v| format!("{v:.0}%")
            ))
            .child(limit!(
                "npos",
                "Max positions",
                max_positions,
                1,
                1,
                50,
                |v| format!("{v:.0}")
            ))
            .child(limit!(
                "hold",
                "Min hold (days)",
                min_hold_days,
                1,
                0,
                60,
                |v| format!("{v:.0}")
            ))
            .child(limit!(
                "nord",
                "Orders per day",
                max_orders_per_day,
                1,
                1,
                100,
                |v| format!("{v:.0}")
            ))
            .child(limit!(
                "conv",
                "Min conviction",
                min_conviction_to_buy,
                0.05,
                0.3,
                0.95,
                |v| format!("{v:.2}")
            ))
            .child(limit!(
                "high",
                "Max below 52w high",
                max_percent_below_52w_high,
                5.0,
                5.0,
                95.0,
                |v| format!("{v:.0}%")
            ))
            .child(limit!(
                "mom",
                "Min 12-1m return",
                min_return_12_1_percent,
                5.0,
                -90.0,
                50.0,
                |v| format!("{v:+.0}%")
            ))
            .child(section("strategy"))
            .child(row("Horizon", segmented(horizon_choices)))
            .child(row("Sectors", segmented(sector_choices)))
            .child(row(
                "Idle cash",
                toggle("sweep", d.cash_sweep.is_some(), "park in T-bills (SGOV)").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.update(cx, |s| {
                            s.cash_sweep = match s.cash_sweep {
                                Some(_) => None,
                                None => Some(crate::agents::sweep::DEFAULT_FUND.into()),
                            }
                        })
                    }),
                ),
            ))
            .child(row(
                "Foreign companies",
                toggle("foreign", d.universe.allow_foreign, "allow (incl. ADRs)").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.update(cx, |s| s.universe.allow_foreign = !s.universe.allow_foreign)
                    }),
                ),
            ))
            .child(stepper!(
                "price",
                "Min price",
                universe.min_price,
                1.0,
                1.0,
                500.0,
                |v| format!("${v:.0}")
            ))
            .child(stepper!(
                "dv",
                "Min daily $ volume",
                universe.min_avg_dollar_volume,
                5_000_000.0,
                1_000_000.0,
                1_000_000_000.0,
                |v| format!("${:.0}M", v / 1e6)
            ))
            .child(row("Excluded tickers", self.excluded.clone()))
            .child(section("schedule"))
            .child(stepper!(
                "cycle",
                "Cycle every",
                schedule.cycle_interval_minutes,
                30,
                30,
                480,
                |v| format!("{v:.0} min")
            ))
            .child(stepper!(
                "explore",
                "Explore every",
                schedule.exploration_interval_hours,
                6,
                6,
                168,
                |v| format!("{v:.0} h")
            ))
            .child(stepper!(
                "cands",
                "Candidates",
                schedule.max_candidates,
                1,
                1,
                15,
                |v| format!("{v:.0}")
            ))
            .child(stepper!(
                "research",
                "Research per cycle",
                schedule.max_research_per_cycle,
                1,
                1,
                15,
                |v| format!("{v:.0}")
            ))
            .child(stepper!(
                "runs",
                "Runs per day",
                schedule.max_agent_runs_per_day,
                5,
                5,
                300,
                |v| format!("{v:.0}")
            ))
            .child(section("instructions"))
            .child(label(
                "Added to every prompt for this agent. Hard limits above always apply.",
            ))
            .child(self.instructions.clone())
            .child(div().h(px(24.)))
    }
}
