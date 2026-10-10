//! The agent list in the sidebar, and creating and editing agents.

use std::sync::Arc;

use gpui::{Context, Div, ElementId, FontWeight, div, prelude::*, px, rgb, transparent_black};

use crate::Warren;
use crate::agent_editor::{AgentEditor, EditorEvent};
use crate::autopilot_ui::AgentsState;
use crate::store::Store;
use crate::strategy::Strategy;
use crate::theme::*;

/// A new agent's budget, if the account has that much unallocated.
const DEFAULT_BUDGET: f64 = 25_000.0;

impl Warren {
    pub fn select_agent(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.agents.len() && index != self.selected_agent {
            self.selected_agent = index;
            // The scorecard and candidates belong to the agent.
            self.scorecard = Default::default();
            self.refresh_quotes(cx);
            cx.notify();
        }
    }

    pub fn open_editor(&mut self, is_new: bool, cx: &mut Context<Self>) {
        let equity = self.portfolio.account.as_ref().map(|a| a.equity);
        let editing = (!is_new).then(|| self.agent()).flatten();
        let draft = match editing {
            Some(agent) => (*agent.ctx.strategy).clone(),
            None => {
                let claimed: f64 = self.agents.iter().map(|a| a.ctx.strategy.budget).sum();
                let free = equity.map_or(DEFAULT_BUDGET, |e| (e - claimed).max(0.0));
                let mut s = Strategy::new("", DEFAULT_BUDGET.min(free).floor());
                s.id = String::new();
                s
            }
        };
        let others: Vec<&AgentsState> = self
            .agents
            .iter()
            .filter(|a| is_new || a.ctx.strategy.id != draft.id)
            .collect();
        let taken_ids = self
            .agents
            .iter()
            .map(|a| a.ctx.strategy.id.clone())
            .collect();
        let other_budgets = others.iter().map(|a| a.ctx.strategy.budget).sum();
        let editor =
            cx.new(|cx| AgentEditor::new(draft, is_new, taken_ids, other_budgets, equity, cx));
        let subscription = cx.subscribe(&editor, |this, _, event, cx| match event {
            EditorEvent::Saved(strategy) => this.save_agent(strategy.clone(), cx),
            EditorEvent::Cancelled => {
                this.editor = None;
                cx.notify();
            }
        });
        self.editor = Some((editor, subscription, is_new));
        cx.notify();
    }

    /// Persist `strategy`, then start it (new) or hand its autopilot the new
    /// settings (edited).
    fn save_agent(&mut self, mut strategy: Strategy, cx: &mut Context<Self>) {
        let is_new = self.editor.as_ref().is_some_and(|(_, _, new)| *new);
        // Whether the agent runs is the AUTO switch's to say, not the form's,
        // which may predate a flip of it.
        if let Some(agent) = self
            .agents
            .iter()
            .find(|a| a.ctx.strategy.id == strategy.id)
        {
            strategy.autostart = !agent.paused();
        }
        let result = (|| -> anyhow::Result<()> {
            let home = self
                .home
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("agents are unavailable"))?;
            home.strategies.save(&strategy)?;
            let config = Arc::new(strategy.config(&home.global));
            if is_new {
                let base = self
                    .agents
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("no running agent to share clients with"))?;
                let store = Arc::new(Store::open(home.strategies.dir(&strategy.id))?);
                let ctx = base.ctx.for_strategy(strategy, store, config);
                self.agents.push(AgentsState::start(ctx));
                self.selected_agent = self.agents.len() - 1;
            } else if let Some(agent) = self
                .agents
                .iter_mut()
                .find(|a| a.ctx.strategy.id == strategy.id)
            {
                let strategy = Arc::new(strategy);
                agent.ctx.strategy = strategy.clone();
                agent.ctx.config = config.clone();
                agent.control.update(strategy, config);
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.editor = None;
                self.scorecard = Default::default();
                self.refresh_quotes(cx);
                self.refresh_portfolio(cx);
            }
            Err(err) => self.agents_error = Some(format!("Saving agent failed: {err:#}").into()),
        }
        cx.notify();
    }

    /// Agents with their state and return, then a button for a new one.
    pub fn render_agent_list(&self, cx: &mut Context<Self>) -> Div {
        let rows = self.agents.iter().enumerate().map(|(i, agent)| {
            let s = &agent.ctx.strategy;
            let selected = i == self.selected_agent;
            let (dot, state) = if agent.paused() {
                (MUTED, "paused")
            } else {
                (GREEN, "auto")
            };
            let ret = agent
                .book
                .as_ref()
                .map(|b| b.equity() / b.budget * 100.0 - 100.0);
            let (ret_text, ret_color) = format_change(ret);
            div()
                .id(ElementId::Name(format!("agent-{}", s.id).into()))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .border_l_2()
                .border_color(transparent_black())
                .cursor_pointer()
                .when(selected, |row| {
                    row.bg(rgb(SELECTED)).border_color(rgb(ACCENT))
                })
                .when(!selected, |row| row.hover(|style| style.bg(rgb(HOVER))))
                .on_click(cx.listener(move |this, _, _, cx| this.select_agent(i, cx)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(s.name.clone()),
                        )
                        .child(div().text_color(ret_color).child(ret_text)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .text_size(px(TEXT_XS))
                        .text_color(rgb(MUTED))
                        .child(div().flex_1().min_w_0().truncate().child(format!(
                            "${:.0}k · {} · {}",
                            s.budget / 1000.0,
                            s.risk.label().to_lowercase(),
                            state
                        )))
                        .when(selected, |r| {
                            r.child(
                                div()
                                    .id(ElementId::Name(format!("edit-{}", s.id).into()))
                                    .flex_none()
                                    .text_color(rgb(ACCENT))
                                    .cursor_pointer()
                                    .child("edit")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.open_editor(false, cx)
                                    })),
                            )
                        }),
                )
        });
        div()
            .flex()
            .flex_col()
            .child(div().px_2().pt_2().pb_1().child(label("Agents")))
            .children(rows)
            .when(self.home.is_some(), |list| {
                list.child(
                    div().px_1().pt_1().child(
                        ghost_button("new-agent", "+ New agent", false)
                            .on_click(cx.listener(|this, _, _, cx| this.open_editor(true, cx))),
                    ),
                )
            })
    }
}
