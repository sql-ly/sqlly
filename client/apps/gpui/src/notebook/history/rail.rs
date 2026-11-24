//! The History tab of the left rail, the run comparison / past run panel
//! under the cell list, and the notebook's test summary in the top bar.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use sqlly_services::analytics::assertions::AssertionStatus;
use sqlly_services::notebook::CellStatus;
use sqlly_services::notebook::run_record::RunRecord;

use super::compare::{CellComparison, ChangeCause, RunComparison};
use super::{details_of, run_tests};
use crate::icon_catalog::AppIcon;
use crate::notebook::rails::{icon_button, text_button};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn note(text: String, color: gpui::Hsla) -> AnyElement {
    div()
        .px(px(Theme::scaled(10.0)))
        .py(px(Theme::scaled(4.0)))
        .text_size(px(Theme::font_size_ratio(0.82)))
        .text_color(color)
        .child(text)
        .into_any_element()
}

pub(crate) fn status_key(status: AssertionStatus) -> &'static str {
    match status {
        AssertionStatus::Pass => "notebook.analysis.assertion.status_pass",
        AssertionStatus::Warn => "notebook.analysis.assertion.status_warn",
        AssertionStatus::Fail => "notebook.analysis.assertion.status_fail",
        AssertionStatus::Error => "notebook.analysis.assertion.status_error",
        AssertionStatus::Inconclusive => "notebook.analysis.assertion.status_inconclusive",
    }
}

pub(crate) fn status_color(status: AssertionStatus) -> gpui::Hsla {
    match status {
        AssertionStatus::Pass => Theme::success(),
        AssertionStatus::Warn | AssertionStatus::Inconclusive => Theme::warning(),
        AssertionStatus::Fail | AssertionStatus::Error => Theme::error(),
    }
}

fn run_time(run: &RunRecord) -> String {
    run.started_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn cause_text(cause: &ChangeCause, view: &NotebookView) -> String {
    match cause {
        ChangeCause::Sql => t("notebook.analysis.compare.cause_sql"),
        ChangeCause::Spec => t("notebook.analysis.compare.cause_spec"),
        ChangeCause::Parameters(names) => crate::localization::format(
            "notebook.analysis.compare.cause_parameters",
            &[("names", names.join(", "))],
        ),
        ChangeCause::Connection => t("notebook.analysis.compare.cause_connection"),
        ChangeCause::Policy => t("notebook.analysis.compare.cause_policy"),
        ChangeCause::Upstream(cells) if cells.is_empty() => {
            t("notebook.analysis.compare.cause_upstream_any")
        }
        ChangeCause::Upstream(cells) => crate::localization::format(
            "notebook.analysis.compare.cause_upstream",
            &[(
                "cells",
                cells
                    .iter()
                    .map(|c| view.display_name(c))
                    .collect::<Vec<_>>()
                    .join(", "),
            )],
        ),
        ChangeCause::Data => t("notebook.analysis.compare.cause_data"),
        ChangeCause::Unknown => t("notebook.analysis.compare.cause_unknown"),
    }
}

fn status_text(status: Option<CellStatus>) -> String {
    match status {
        None => t("notebook.analysis.compare.not_run"),
        Some(CellStatus::Failed) => t("notebook.analysis.compare.failed"),
        Some(_) => t("notebook.analysis.compare.ok"),
    }
}

/// One line explaining a cell's change between two runs.
pub(crate) fn explain_cell(cell: &CellComparison, view: &NotebookView) -> String {
    if cell.before.is_none() {
        return t("notebook.analysis.compare.only_after");
    }
    if cell.after.is_none() {
        return t("notebook.analysis.compare.only_before");
    }
    if cell.unchanged() {
        return if cell.data_conclusive {
            t("notebook.analysis.compare.unchanged_proven")
        } else {
            t("notebook.analysis.compare.unchanged")
        };
    }
    let mut parts: Vec<String> = cell.causes.iter().map(|c| cause_text(c, view)).collect();
    if parts.is_empty() && cell.result_changed {
        parts.push(t("notebook.analysis.compare.cause_data"));
    }
    if !cell.causes.is_empty() && !cell.result_changed && cell.data_conclusive {
        parts.push(t("notebook.analysis.compare.same_result"));
    }
    parts.join(" · ")
}

impl NotebookView {
    pub(in crate::notebook) fn history_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out: Vec<AnyElement> = Vec::new();
        if !self.analysis.history.is_enabled() {
            out.push(note(
                t("notebook.analysis.history.disabled"),
                Theme::text_dim(),
            ));
            return out;
        }
        let input = self.ensure_snapshot_input(window, cx);
        let active = self.active.clone();
        out.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .px(px(s(10.0)))
                .py(px(s(4.0)))
                .child(div().flex_1().child(Input::new(&input).small()))
                .child(
                    text_button("nb-history-pin", t("notebook.analysis.history.pin"))
                        .disabled(active.is_none())
                        .tooltip(t("notebook.analysis.history.pin_tooltip"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let Some(cell) = active.clone() else {
                                return;
                            };
                            let Some(input) = this.analysis.history.snapshot_name.clone() else {
                                return;
                            };
                            let name = input.read(cx).value().to_string();
                            if this.pin_snapshot(&cell, &name, cx) {
                                input.update(cx, |s, cx| s.set_value("", window, cx));
                            }
                        })),
                )
                .into_any_element(),
        );
        if let Some(error) = self.analysis.history.error.clone() {
            out.push(note(
                crate::localization::format(
                    "notebook.analysis.history.outputs_error",
                    &[("error", crate::localization::message(&error))],
                ),
                Theme::warning(),
            ));
        }
        let selected = self.analysis.history.selected.clone();
        out.push(
            div()
                .flex()
                .flex_row()
                .gap(px(s(4.0)))
                .px(px(s(8.0)))
                .py(px(s(2.0)))
                .child(
                    text_button("nb-history-compare", t("notebook.analysis.history.compare"))
                        .disabled(selected.len() != 2)
                        .on_click(cx.listener(|this, _, _, cx| this.compare_selected_runs(cx))),
                )
                .child(
                    text_button("nb-history-open", t("notebook.analysis.history.open"))
                        .disabled(selected.len() != 1)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(run) = this.analysis.history.selected.first().cloned() {
                                this.open_run(&run, cx);
                            }
                        })),
                )
                .into_any_element(),
        );
        let runs = self.recorded_runs().to_vec();
        if runs.is_empty() {
            out.push(note(
                t("notebook.analysis.history.empty"),
                Theme::text_dim(),
            ));
        }
        for (n, run) in runs.iter().rev().enumerate() {
            out.push(self.run_row(n, run, selected.contains(&run.run_id), cx));
        }
        let snapshots = match (self.analysis.history.stores(), self.trust.scope.as_deref()) {
            (Some(s), Some(scope)) => s.snapshots(scope),
            _ => Vec::new(),
        };
        if !snapshots.is_empty() {
            out.push(note(
                t("notebook.analysis.history.snapshots"),
                Theme::text(),
            ));
            for snap in snapshots {
                out.push(note(
                    crate::localization::format(
                        if snap.row_count == 1 {
                            "notebook.analysis.history.snapshot_row_one"
                        } else {
                            "notebook.analysis.history.snapshot_row"
                        },
                        &[
                            ("name", snap.name.as_str()),
                            ("cell", &self.display_name(&snap.cell_id)),
                            ("count", &snap.row_count.to_string()),
                        ],
                    ),
                    Theme::text_dim(),
                ));
            }
        }
        out
    }

    fn run_row(
        &self,
        n: usize,
        run: &RunRecord,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let details = details_of(run);
        let failed = run
            .cells
            .iter()
            .filter(|c| c.status == CellStatus::Failed)
            .count();
        let ok = run.cells.len() - failed;
        let tests = run_tests(&details);
        let params: Vec<String> = details
            .parameters
            .iter()
            .map(|p| {
                let value = if p.redacted {
                    t("notebook.analysis.history.hidden_value")
                } else {
                    p.value
                        .as_ref()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default()
                };
                format!("{}={value}", p.name)
            })
            .collect();
        let mode = t(match details.request.as_str() {
            "cell" => "notebook.analysis.history.request_cell",
            "cell_with_inputs" => "notebook.analysis.history.request_cell_with_inputs",
            "all" => "notebook.analysis.history.request_all",
            "stale" => "notebook.analysis.history.request_stale",
            "downstream" => "notebook.analysis.history.request_downstream",
            "above" => "notebook.analysis.history.request_above",
            "below" => "notebook.analysis.history.request_below",
            "selected" => "notebook.analysis.history.request_selected",
            _ => "notebook.analysis.history.request_other",
        });
        let summary = crate::localization::format(
            "notebook.analysis.history.run_summary",
            &[("ok", ok.to_string()), ("failed", failed.to_string())],
        );
        let tests_text = (tests.total() > 0).then(|| {
            crate::localization::format(
                if tests.total() == 1 {
                    "notebook.analysis.tests.summary_one"
                } else {
                    "notebook.analysis.tests.summary"
                },
                &[
                    ("failing", tests.failing().to_string()),
                    ("total", tests.total().to_string()),
                ],
            )
        });
        let run_id = run.run_id.clone();
        div()
            .id(("nb-history-run", n))
            .flex()
            .flex_col()
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .cursor_pointer()
            .when(selected, |d| d.bg(Theme::selection_fill()))
            .hover(|style| style.bg(Theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_run_selected(&run_id, cx)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "notebook.analysis.history.run_heading",
                        &[("time", run_time(run)), ("mode", mode)],
                    )),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(if failed > 0 {
                        Theme::error()
                    } else {
                        Theme::text_dim()
                    })
                    .child(summary),
            )
            .when_some(tests_text, |d, text| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(if tests.failing() > 0 {
                            Theme::error()
                        } else {
                            Theme::success()
                        })
                        .child(text),
                )
            })
            .when(!params.is_empty(), |d| {
                d.child(
                    div()
                        .truncate()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::text_dim())
                        .child(params.join("  ")),
                )
            })
            .into_any_element()
    }

    /// The comparison or past run panel under the cell list.
    pub(in crate::notebook) fn render_history_panel(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = Theme::scaled;
        let (title, body): (String, Vec<AnyElement>) =
            if let Some(cmp) = self.analysis.history.comparison.clone() {
                (self.comparison_title(&cmp), self.comparison_rows(&cmp))
            } else if let Some(opened) = &self.analysis.history.opened {
                let title = crate::localization::format(
                    "notebook.analysis.history.opened_title",
                    &[("run", opened.run_id.as_str())],
                );
                let rows = opened
                    .cells
                    .iter()
                    .map(|(cell, grid, rows)| {
                        let label = crate::localization::format(
                            if *rows == Some(1) {
                                "notebook.analysis.history.opened_cell_one"
                            } else {
                                "notebook.analysis.history.opened_cell"
                            },
                            &[
                                ("cell", self.display_name(cell)),
                                ("count", rows.map(|r| r.to_string()).unwrap_or_default()),
                            ],
                        );
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(2.0)))
                            .child(note(label, Theme::text()))
                            .child(match grid {
                                Some(grid) => div()
                                    .h(px(s(150.0)))
                                    .mx(px(s(10.0)))
                                    .child(grid.clone())
                                    .into_any_element(),
                                None => note(
                                    t("notebook.analysis.history.output_missing"),
                                    Theme::text_dim(),
                                ),
                            })
                            .into_any_element()
                    })
                    .collect();
                (title, rows)
            } else {
                return None;
            };
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .max_h(px(s(360.0)))
                .border_t_1()
                .border_color(Theme::border())
                .bg(Theme::panel_bg())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .px(px(s(10.0)))
                        .py(px(s(4.0)))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Theme::font_size_ratio(0.95)))
                                .text_color(Theme::text())
                                .child(title),
                        )
                        .child(
                            icon_button(
                                "nb-history-close",
                                AppIcon::Close,
                                t("notebook.analysis.history.close"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.close_history_panel(cx))),
                        ),
                )
                .child(
                    div()
                        .id("nb-history-panel")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pb(px(s(6.0)))
                        .children(body),
                )
                .into_any_element(),
        )
    }

    fn comparison_title(&mut self, cmp: &RunComparison) -> String {
        let runs = self.recorded_runs();
        let time = |id: &str| {
            runs.iter()
                .find(|r| r.run_id == id)
                .map(run_time)
                .unwrap_or_else(|| id.to_string())
        };
        crate::localization::format(
            "notebook.analysis.compare.title",
            &[("before", time(&cmp.before)), ("after", time(&cmp.after))],
        )
    }

    fn comparison_rows(&self, cmp: &RunComparison) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if cmp.notebook_changed {
            out.push(note(
                t("notebook.analysis.compare.notebook_changed"),
                Theme::text_dim(),
            ));
        }
        for p in &cmp.params {
            let show = |v: &Option<serde_json::Value>| match v {
                _ if p.hidden => t("notebook.analysis.history.hidden_value"),
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => t("notebook.analysis.compare.unset"),
            };
            out.push(note(
                crate::localization::format(
                    "notebook.analysis.compare.param_row",
                    &[
                        ("name", p.name.clone()),
                        ("before", show(&p.before)),
                        ("after", show(&p.after)),
                    ],
                ),
                Theme::text(),
            ));
        }
        for cell in &cmp.cells {
            let rows = match (cell.rows_before, cell.rows_after) {
                (Some(b), Some(a)) if a != b => crate::localization::format(
                    "notebook.analysis.compare.rows_delta",
                    &[
                        ("before", b.to_string()),
                        ("after", a.to_string()),
                        ("delta", format!("{:+}", a as i64 - b as i64)),
                    ],
                ),
                (_, Some(a)) => crate::localization::format(
                    if a == 1 {
                        "notebook.analysis.compare.rows_same_one"
                    } else {
                        "notebook.analysis.compare.rows_same"
                    },
                    &[("count", a.to_string())],
                ),
                _ => String::new(),
            };
            let header = format!(
                "{} — {} → {}  {rows}",
                self.display_name(&cell.cell_id),
                status_text(cell.before),
                status_text(cell.after)
            );
            out.push(note(header, Theme::text()));
            out.push(note(
                explain_cell(cell, self),
                if cell.unchanged() {
                    Theme::text_dim()
                } else {
                    Theme::warning()
                },
            ));
            for test in &cell.tests {
                let name = |s: Option<AssertionStatus>| {
                    s.map(|s| t(status_key(s)))
                        .unwrap_or_else(|| t("notebook.analysis.compare.not_run"))
                };
                out.push(note(
                    crate::localization::format(
                        "notebook.analysis.compare.test_row",
                        &[
                            ("name", test.name.clone()),
                            ("before", name(test.before)),
                            ("after", name(test.after)),
                        ],
                    ),
                    test.after.map(status_color).unwrap_or_else(Theme::text_dim),
                ));
            }
        }
        out
    }

    /// "3 tests failing" (or passing) in the top bar.
    pub(in crate::notebook) fn render_test_summary(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let summary = self.test_summary();
        if summary.total() == 0 {
            return None;
        }
        let failing = summary.failing();
        let (text, color) = if failing > 0 {
            (
                crate::localization::format(
                    if failing == 1 {
                        "notebook.analysis.tests.failing_one"
                    } else {
                        "notebook.analysis.tests.failing"
                    },
                    &[("count", failing.to_string())],
                ),
                Theme::error(),
            )
        } else if summary.warned + summary.inconclusive > 0 {
            (
                crate::localization::format(
                    if summary.warned + summary.inconclusive == 1 {
                        "notebook.analysis.tests.attention_one"
                    } else {
                        "notebook.analysis.tests.attention"
                    },
                    &[("count", (summary.warned + summary.inconclusive).to_string())],
                ),
                Theme::warning(),
            )
        } else {
            (
                crate::localization::format(
                    if summary.passed == 1 {
                        "notebook.analysis.tests.passing_one"
                    } else {
                        "notebook.analysis.tests.passing"
                    },
                    &[("count", summary.passed.to_string())],
                ),
                Theme::success(),
            )
        };
        let first_failing = self
            .analysis
            .tests
            .iter()
            .find(|(_, s)| s.failing() > 0)
            .map(|(id, _)| id.clone());
        Some(
            div()
                .id("nb-test-summary")
                .flex_none()
                .px(px(Theme::scaled(6.0)))
                .rounded(px(Theme::scaled(3.0)))
                .border_1()
                .border_color(color)
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(color)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(id) = first_failing.clone() {
                        this.reveal_cell(&id, cx);
                    }
                }))
                .child(text)
                .into_any_element(),
        )
    }
}
