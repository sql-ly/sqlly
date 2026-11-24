//! Profile cells: column cards (type, missing values, distinct values,
//! range, average and spread, percentiles, a histogram, top values, likely
//! meaning, outliers, key candidates) and a sortable summary grid over the
//! input result, labelled complete, estimated or partial.

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::analytics::profile::{
    ColumnProfile, KeyCandidate, ResultProfile, SemanticType,
};
use sqlly_services::notebook::cells::profile::{
    Coverage, ProfileCellSpec, ProfileSort, WholeColumn, WholeTable, candidate_keys,
    column_has_estimates, sorted_columns,
};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::analysis::ui::{
    self, body_text, caption, chip, field, grid_box, number, percent, picker, t,
};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct ProfileCell;

impl NotebookCell for ProfileCell {
    fn kind(&self) -> &'static str {
        crate::notebook::analysis::KIND_PROFILE
    }

    fn label_key(&self) -> &'static str {
        "notebook.analysis.kind.profile"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        ui::cell_body(cell)
    }
}

/// Columns shown as cards at once.
pub const MAX_CARDS: usize = 40;

pub(crate) fn semantic_key(semantic: SemanticType) -> &'static str {
    match semantic {
        SemanticType::Integer => "notebook.analysis.profile.semantic.integer",
        SemanticType::Decimal => "notebook.analysis.profile.semantic.decimal",
        SemanticType::Boolean => "notebook.analysis.profile.semantic.boolean",
        SemanticType::Date => "notebook.analysis.profile.semantic.date",
        SemanticType::DateTime => "notebook.analysis.profile.semantic.date_time",
        SemanticType::Time => "notebook.analysis.profile.semantic.time",
        SemanticType::Uuid => "notebook.analysis.profile.semantic.uuid",
        SemanticType::Email => "notebook.analysis.profile.semantic.email",
        SemanticType::Url => "notebook.analysis.profile.semantic.url",
        SemanticType::Json => "notebook.analysis.profile.semantic.json",
        SemanticType::Ip => "notebook.analysis.profile.semantic.ip",
        SemanticType::CurrencyLike => "notebook.analysis.profile.semantic.currency",
        SemanticType::Categorical => "notebook.analysis.profile.semantic.categorical",
        SemanticType::FreeText => "notebook.analysis.profile.semantic.free_text",
        SemanticType::IdLike => "notebook.analysis.profile.semantic.id",
        SemanticType::Text => "notebook.analysis.profile.semantic.text",
        SemanticType::Binary => "notebook.analysis.profile.semantic.binary",
        SemanticType::Empty => "notebook.analysis.profile.semantic.empty",
    }
}

fn sort_key(sort: ProfileSort) -> &'static str {
    match sort {
        ProfileSort::Position => "notebook.analysis.profile.sort_position",
        ProfileSort::Name => "notebook.analysis.profile.sort_name",
        ProfileSort::NullRate => "notebook.analysis.profile.sort_null_rate",
        ProfileSort::Distinct => "notebook.analysis.profile.sort_distinct",
        ProfileSort::Type => "notebook.analysis.profile.sort_type",
    }
}

fn key_key(key: KeyCandidate) -> &'static str {
    match key {
        KeyCandidate::Unique => "notebook.analysis.profile.key_unique",
        KeyCandidate::LikelyUnique => "notebook.analysis.profile.key_likely",
        KeyCandidate::NotUnique => "notebook.analysis.profile.key_not_unique",
    }
}

/// The honest label of how much of the input a profile read.
pub(crate) fn coverage_text(coverage: Coverage) -> String {
    match coverage {
        Coverage::Complete { rows } => crate::localization::format(
            if rows == 1 {
                "notebook.analysis.profile.coverage_complete_one"
            } else {
                "notebook.analysis.profile.coverage_complete"
            },
            &[("count", rows.to_string())],
        ),
        Coverage::CompleteEstimated { rows } => crate::localization::format(
            if rows == 1 {
                "notebook.analysis.profile.coverage_estimated_one"
            } else {
                "notebook.analysis.profile.coverage_estimated"
            },
            &[("count", rows.to_string())],
        ),
        Coverage::Partial {
            read,
            total: Some(total),
        } => crate::localization::format(
            "notebook.analysis.profile.coverage_partial_of",
            &[("count", read.to_string()), ("total", total.to_string())],
        ),
        Coverage::Partial { read, total: None } => crate::localization::format(
            if read == 1 {
                "notebook.analysis.profile.coverage_partial_one"
            } else {
                "notebook.analysis.profile.coverage_partial"
            },
            &[("count", read.to_string())],
        ),
        Coverage::Mixed { read, total } => crate::localization::format(
            "notebook.fu_final.profile.coverage_mixed",
            &[("count", read.to_string()), ("total", total.to_string())],
        ),
    }
}

/// Whether `whole`'s range and averages describe column `c` as the
/// snapshot does (numbers and dates; text ranges are lengths there).
fn whole_range_fits(c: &ColumnProfile) -> bool {
    c.numeric.as_ref().is_some_and(|n| n.count > 0)
        || c.temporal.as_ref().is_some_and(|t| t.count > 0)
}

fn whole_range(c: &ColumnProfile, w: &WholeColumn) -> (Option<String>, Option<String>) {
    let shown = |v: &Option<String>| {
        v.as_ref().map(|text| {
            if c.numeric.is_some() {
                text.trim().parse::<f64>().map_or(text.clone(), number)
            } else {
                text.clone()
            }
        })
    };
    (shown(&w.min), shown(&w.max))
}

/// The summary grid: one row per column in the chosen order.
pub(crate) fn summary_table(
    profile: &ResultProfile,
    whole: Option<&WholeTable>,
    order: &[usize],
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let col = |key: &str, ty: &str| ResultColumn {
        name: t(key),
        data_type: ty.into(),
        nullable: true,
    };
    let columns = vec![
        col("notebook.analysis.profile.col_column", "nvarchar"),
        col("notebook.analysis.profile.col_type", "nvarchar"),
        col("notebook.analysis.profile.col_meaning", "nvarchar"),
        col("notebook.analysis.profile.col_nulls", "float"),
        col("notebook.analysis.profile.col_distinct", "bigint"),
        col("notebook.analysis.profile.col_min", "nvarchar"),
        col("notebook.analysis.profile.col_max", "nvarchar"),
        col("notebook.analysis.profile.col_mean", "float"),
        col("notebook.analysis.profile.col_stddev", "float"),
        col("notebook.analysis.profile.col_key", "nvarchar"),
    ];
    let finite = |v: f64| {
        if v.is_finite() {
            CellValue::F64(v)
        } else {
            CellValue::Null
        }
    };
    let rows = order
        .iter()
        .filter_map(|i| profile.columns.get(*i))
        .map(|c| {
            let w = whole.and_then(|w| w.column(&c.name));
            let (min, max) = match w {
                Some(w) if whole_range_fits(c) => whole_range(c, w),
                _ => range_text(c),
            };
            let (null_rate, distinct) = match w {
                Some(w) => (w.null_rate(), w.distinct),
                None => (c.null_rate, c.distinct.value),
            };
            let (mean, stddev) = match (w, &c.numeric) {
                (Some(w), Some(_)) => (w.mean, w.stddev),
                (_, Some(n)) => (Some(n.mean), Some(n.stddev)),
                _ => (None, None),
            };
            vec![
                CellValue::String(c.name.clone()),
                CellValue::String(c.data_type.clone()),
                CellValue::String(t(semantic_key(c.semantic_type))),
                CellValue::F64((null_rate * 1000.0).round() / 10.0),
                CellValue::I64(distinct as i64),
                min.map_or(CellValue::Null, CellValue::String),
                max.map_or(CellValue::Null, CellValue::String),
                mean.map_or(CellValue::Null, finite),
                stddev.map_or(CellValue::Null, finite),
                CellValue::String(t(key_key(c.candidate_key))),
            ]
        })
        .collect();
    (columns, rows)
}

fn range_text(c: &ColumnProfile) -> (Option<String>, Option<String>) {
    if let Some(n) = &c.numeric
        && n.count > 0
    {
        return (Some(number(n.min)), Some(number(n.max)));
    }
    if let Some(tm) = &c.temporal
        && tm.count > 0
    {
        return (Some(tm.min.clone()), Some(tm.max.clone()));
    }
    if let Some(tx) = &c.text
        && tx.count > 0
    {
        return (
            Some(crate::localization::format(
                if tx.min_length == 1 {
                    "notebook.analysis.profile.length_one"
                } else {
                    "notebook.analysis.profile.length"
                },
                &[("count", tx.min_length.to_string())],
            )),
            Some(crate::localization::format(
                if tx.max_length == 1 {
                    "notebook.analysis.profile.length_one"
                } else {
                    "notebook.analysis.profile.length"
                },
                &[("count", tx.max_length.to_string())],
            )),
        );
    }
    (None, None)
}

/// Histogram bars for a column: numeric bins, else date buckets.
pub(crate) fn histogram_bars(c: &ColumnProfile) -> Vec<(String, f64)> {
    if let Some(n) = &c.numeric
        && n.count > 0
    {
        return n
            .histogram
            .counts
            .iter()
            .enumerate()
            .map(|(i, count)| {
                let lo = n.histogram.edges.get(i).copied().unwrap_or(f64::NAN);
                (number(lo), *count as f64)
            })
            .collect();
    }
    if let Some(tm) = &c.temporal {
        return tm
            .buckets
            .iter()
            .map(|b| (b.start.clone(), b.count as f64))
            .collect();
    }
    Vec::new()
}

/// The source line heading one part of a column card.
fn section_note(text: String) -> AnyElement {
    div()
        .pt(px(Theme::scaled(2.0)))
        .text_size(px(Theme::font_size_ratio(0.75)))
        .text_color(Theme::text_dim())
        .child(text)
        .into_any_element()
}

struct Shown {
    profile: ResultProfile,
    /// Column statistics over every row of the local table the input is.
    whole: Option<WholeTable>,
    coverage: Coverage,
    order: Vec<usize>,
    histograms: Vec<Option<String>>,
    grid: Option<Entity<SqllyDataTable>>,
    sort: ProfileSort,
    descending: bool,
}

pub struct ProfileView {
    notebook: WeakEntity<NotebookView>,
    cell_id: String,
    shown: Option<Shown>,
    total: u64,
}

impl ProfileView {
    pub fn new(notebook: WeakEntity<NotebookView>, cell_id: String) -> Self {
        Self {
            notebook,
            cell_id,
            shown: None,
            total: 0,
        }
    }

    #[cfg(test)]
    pub fn profile(&self) -> Option<&ResultProfile> {
        self.shown.as_ref().map(|s| &s.profile)
    }

    #[cfg(test)]
    pub fn coverage(&self) -> Option<Coverage> {
        self.shown.as_ref().map(|s| s.coverage)
    }

    #[cfg(test)]
    pub fn whole(&self) -> Option<&WholeTable> {
        self.shown.as_ref().and_then(|s| s.whole.as_ref())
    }

    #[cfg(test)]
    pub fn summary(&self) -> Option<(Vec<ResultColumn>, Vec<Vec<CellValue>>)> {
        self.shown
            .as_ref()
            .map(|s| summary_table(&s.profile, s.whole.as_ref(), &s.order))
    }

    pub fn set_profile(
        &mut self,
        profile: ResultProfile,
        whole: Option<WholeTable>,
        total: u64,
        spec: &ProfileCellSpec,
        cx: &mut Context<Self>,
    ) {
        self.total = total;
        let coverage = Coverage::of(&profile, Some(total)).with_whole(&profile, whole.as_ref());
        let whole = matches!(coverage, Coverage::Mixed { .. })
            .then_some(whole)
            .flatten();
        let histograms = profile
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let bars = histogram_bars(c);
                (bars.len() > 1).then(|| ui::bar_chart_svg(&format!("h{i}"), &bars, 240.0, 72.0))
            })
            .collect();
        self.shown = Some(Shown {
            profile,
            whole,
            coverage,
            order: Vec::new(),
            histograms,
            grid: None,
            sort: ProfileSort::Position,
            descending: false,
        });
        self.apply_sort(spec, true, cx);
        cx.notify();
    }

    fn resort(&mut self, cx: &mut Context<Self>, force: bool) {
        let spec = self.spec(cx);
        self.apply_sort(&spec, force, cx);
    }

    fn apply_sort(&mut self, spec: &ProfileCellSpec, force: bool, cx: &mut Context<Self>) {
        let Some(shown) = self.shown.as_mut() else {
            return;
        };
        if !force && shown.sort == spec.sort && shown.descending == spec.descending {
            return;
        }
        shown.sort = spec.sort;
        shown.descending = spec.descending;
        shown.order = sorted_columns(&shown.profile, spec.sort, spec.descending);
        let (columns, rows) = summary_table(&shown.profile, shown.whole.as_ref(), &shown.order);
        shown.grid = ui::grid(&columns, &rows, cx);
    }

    fn spec(&self, cx: &gpui::App) -> ProfileCellSpec {
        self.notebook
            .upgrade()
            .map(|nb| ProfileCellSpec::from_spec(&nb.read(cx).doc.spec(&self.cell_id)))
            .unwrap_or_default()
    }

    fn edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut ProfileCellSpec)) {
        Self::edit_at(&self.notebook, &self.cell_id, cx, change);
    }

    fn edit_view(
        view: &Entity<Self>,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut ProfileCellSpec),
    ) {
        let (notebook, id) = {
            let v = view.read(cx);
            (v.notebook.clone(), v.cell_id.clone())
        };
        Self::edit_at(&notebook, &id, cx, change);
    }

    fn edit_at(
        notebook: &WeakEntity<NotebookView>,
        id: &str,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut ProfileCellSpec),
    ) {
        let Some(nb) = notebook.upgrade() else {
            return;
        };
        nb.update(cx, |nb, cx| {
            let mut spec = ProfileCellSpec::from_spec(&nb.doc.spec(id));
            change(&mut spec);
            nb.set_cell_spec(id, spec.to_spec(), cx);
        });
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let spec = self.spec(cx);
        let (choices, running) = self
            .notebook
            .upgrade()
            .map(|nb| {
                let nb = nb.read(cx);
                (
                    nb.input_choices(&self.cell_id),
                    nb.cell_state(&self.cell_id).is_active(),
                )
            })
            .unwrap_or_default();
        let current = choices
            .iter()
            .find(|(id, label)| *id == spec.input || label.eq_ignore_ascii_case(&spec.input))
            .map(|(id, label)| (id.clone(), label.clone()));
        let this = cx.weak_entity();
        let pick_input: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |value, app| {
                if let Some(view) = this.upgrade() {
                    Self::edit_view(&view, app, |s| s.input = value);
                }
            }
        });
        let pick_sort: ui::OnChoose = std::rc::Rc::new(move |value, app| {
            if let Some(view) = this.upgrade() {
                let sort = ProfileSort::ALL
                    .into_iter()
                    .find(|s| s.as_str() == value)
                    .unwrap_or_default();
                Self::edit_view(&view, app, |s| s.sort = sort);
                view.update(app, |v, cx| {
                    v.resort(cx, false);
                    cx.notify();
                });
            }
        });
        let descending = spec.descending;
        let notebook = self.notebook.clone();
        let run_id = self.cell_id.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::scaled(8.0)))
            .child(field(
                t("notebook.analysis.input_label"),
                picker(
                    "nb-profile-input",
                    current
                        .as_ref()
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| t("notebook.analysis.input_choose")),
                    choices,
                    current.map(|(id, _)| id),
                    pick_input,
                ),
            ))
            .child(field(
                t("notebook.analysis.profile.sort_label"),
                picker(
                    "nb-profile-sort",
                    t(sort_key(spec.sort)),
                    ProfileSort::ALL
                        .iter()
                        .map(|s| (s.as_str().to_string(), t(sort_key(*s))))
                        .collect(),
                    Some(spec.sort.as_str().to_string()),
                    pick_sort,
                ),
            ))
            .child(
                ui::button(
                    "nb-profile-desc",
                    t(if descending {
                        "notebook.analysis.profile.descending"
                    } else {
                        "notebook.analysis.profile.ascending"
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.edit(cx, |s| s.descending = !s.descending);
                    this.resort(cx, false);
                    cx.notify();
                })),
            )
            .child(
                ui::primary_button("nb-profile-run", t("notebook.analysis.run"))
                    .disabled(running)
                    .on_click(move |_, _, app| {
                        if let Some(nb) = notebook.upgrade() {
                            nb.update(app, |nb, cx| nb.request_run_from_host(&run_id, cx));
                        }
                    }),
            )
            .into_any_element()
    }

    fn card(&self, shown: &Shown, index: usize) -> AnyElement {
        let s = Theme::scaled;
        let c = &shown.profile.columns[index];
        let whole = shown.whole.as_ref().and_then(|w| w.column(&c.name));
        let approx = |flag: bool| if flag { "≈ " } else { "" };
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(w) = whole {
            rows.push(section_note(t("notebook.fu_final.profile.whole_table")));
            rows.push(
                caption(crate::localization::format(
                    "notebook.analysis.profile.nulls",
                    &[
                        ("count", w.null_count.to_string()),
                        ("rate", percent(w.null_rate())),
                    ],
                ))
                .into_any_element(),
            );
            rows.push(
                caption(format!(
                    "{}{}",
                    approx(true),
                    crate::localization::format(
                        "notebook.analysis.profile.distinct",
                        &[("count", w.distinct.to_string())],
                    )
                ))
                .into_any_element(),
            );
            if whole_range_fits(c)
                && let (Some(min), Some(max)) = whole_range(c, w)
            {
                rows.push(
                    caption(crate::localization::format(
                        "notebook.analysis.profile.range",
                        &[("min", min), ("max", max)],
                    ))
                    .into_any_element(),
                );
            }
            if c.numeric.is_some() {
                if let (Some(mean), Some(sd)) = (w.mean, w.stddev) {
                    rows.push(
                        caption(crate::localization::format(
                            "notebook.analysis.profile.mean_sd",
                            &[("mean", number(mean)), ("sd", number(sd))],
                        ))
                        .into_any_element(),
                    );
                }
                if let (Some(q1), Some(median), Some(q3)) = (w.q25, w.q50, w.q75) {
                    rows.push(
                        caption(format!(
                            "{}{}",
                            approx(true),
                            crate::localization::format(
                                "notebook.fu_final.profile.quartiles",
                                &[
                                    ("q1", number(q1)),
                                    ("median", number(median)),
                                    ("q3", number(q3)),
                                ],
                            )
                        ))
                        .into_any_element(),
                    );
                }
            }
            rows.push(section_note(crate::localization::format(
                "notebook.fu_final.profile.first_rows",
                &[("count", shown.profile.row_count.to_string())],
            )));
        } else {
            rows.push(
                caption(crate::localization::format(
                    "notebook.analysis.profile.nulls",
                    &[
                        ("count", c.null_count.to_string()),
                        ("rate", percent(c.null_rate)),
                    ],
                ))
                .into_any_element(),
            );
            rows.push(
                caption(format!(
                    "{}{}",
                    approx(c.distinct.approximate),
                    crate::localization::format(
                        "notebook.analysis.profile.distinct",
                        &[("count", c.distinct.value.to_string())],
                    )
                ))
                .into_any_element(),
            );
        }
        let (min, max) = range_text(c);
        if (whole.is_none() || !whole_range_fits(c))
            && let (Some(min), Some(max)) = (min, max)
        {
            rows.push(
                caption(crate::localization::format(
                    "notebook.analysis.profile.range",
                    &[("min", min), ("max", max)],
                ))
                .into_any_element(),
            );
        }
        if let Some(n) = &c.numeric
            && n.count > 0
        {
            if whole.is_none() {
                rows.push(
                    caption(crate::localization::format(
                        "notebook.analysis.profile.mean_sd",
                        &[("mean", number(n.mean)), ("sd", number(n.stddev))],
                    ))
                    .into_any_element(),
                );
            }
            rows.push(
                caption(format!(
                    "{}{}",
                    approx(n.percentiles.approximate),
                    crate::localization::format(
                        "notebook.analysis.profile.percentiles",
                        &[
                            ("low", number(n.percentiles.p5)),
                            ("median", number(n.percentiles.p50)),
                            ("high", number(n.percentiles.p95)),
                        ],
                    )
                ))
                .into_any_element(),
            );
            if n.outlier_count > 0 {
                rows.push(
                    caption(format!(
                        "{}{}",
                        approx(n.outliers_approximate),
                        crate::localization::format(
                            "notebook.analysis.profile.outliers",
                            &[("count", n.outlier_count.to_string())],
                        )
                    ))
                    .into_any_element(),
                );
            }
        }
        if let Some(Some(svg)) = shown.histograms.get(index) {
            rows.push(ui::svg_image(svg.clone(), 240.0, 72.0));
        }
        if !c.top_values.is_empty() {
            rows.push(
                caption(format!(
                    "{}{}",
                    approx(c.top_values_approximate),
                    t("notebook.analysis.profile.top_values")
                ))
                .into_any_element(),
            );
            for v in c.top_values.iter().take(5) {
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(6.0)))
                        .child(body_text(v.value.clone()).flex_1().truncate())
                        .child(caption(v.count.to_string()))
                        .into_any_element(),
                );
            }
        }
        div()
            .w(px(s(264.0)))
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .p(px(s(8.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(body_text(c.name.clone()).truncate())
                    .child(caption(c.data_type.clone()))
                    .child(chip(t(semantic_key(c.semantic_type)), Theme::accent()))
                    .when(c.candidate_key != KeyCandidate::NotUnique, |d| {
                        d.child(chip(t(key_key(c.candidate_key)), Theme::success()))
                    })
                    .when(column_has_estimates(c), |d| {
                        d.child(chip(
                            t("notebook.analysis.profile.estimated"),
                            Theme::warning(),
                        ))
                    }),
            )
            .children(rows)
            .into_any_element()
    }
}

impl Render for ProfileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resort(cx, false);
        let s = Theme::scaled;
        let output_only =
            crate::notebook::present::cell_output_only(&self.notebook, &self.cell_id, cx);
        let toolbar = self.toolbar(cx);
        let mut root = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .w_full()
            .when(!output_only, |d| d.child(toolbar));
        let Some(shown) = &self.shown else {
            return root.child(caption(t("notebook.analysis.profile.empty")));
        };
        let color = if shown.coverage.is_complete() {
            Theme::text_dim()
        } else {
            Theme::warning()
        };
        root = root.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(color)
                .child(coverage_text(shown.coverage)),
        );
        let keys = candidate_keys(&shown.profile);
        if !keys.is_empty() {
            root = root.child(caption(crate::localization::format(
                "notebook.analysis.profile.candidate_keys",
                &[(
                    "columns",
                    keys.iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )],
            )));
        }
        let height = (shown.order.len() as f32 * 24.0 + 32.0).clamp(80.0, 260.0);
        root = root.child(grid_box(shown.grid.clone(), height));
        let cards: Vec<AnyElement> = shown
            .order
            .iter()
            .take(MAX_CARDS)
            .map(|i| self.card(shown, *i))
            .collect();
        root.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(s(8.0)))
                .children(cards),
        )
        .when(shown.order.len() > MAX_CARDS, |d| {
            d.child(caption(crate::localization::format(
                if shown.order.len() - MAX_CARDS == 1 {
                    "notebook.analysis.profile.more_columns_one"
                } else {
                    "notebook.analysis.profile.more_columns"
                },
                &[("count", (shown.order.len() - MAX_CARDS).to_string())],
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::analytics::Completeness;
    use sqlly_services::analytics::profile::profile_rows;

    fn columns() -> Vec<ResultColumn> {
        ["id", "amount", "day"]
            .iter()
            .zip(["int", "decimal(10,2)", "date"])
            .map(|(n, ty)| ResultColumn {
                name: n.to_string(),
                data_type: ty.to_string(),
                nullable: true,
            })
            .collect()
    }

    fn rows(n: i64) -> Vec<Vec<CellValue>> {
        (0..n)
            .map(|i| {
                vec![
                    CellValue::I64(i),
                    if i % 10 == 0 {
                        CellValue::Null
                    } else {
                        CellValue::F64(i as f64 * 1.5)
                    },
                    CellValue::String(format!("2026-01-{:02}", 1 + i % 28)),
                ]
            })
            .collect()
    }

    #[test]
    fn summary_rows_follow_the_sort_and_histograms_have_bars() {
        let profile = profile_rows(
            &columns(),
            &rows(50),
            Completeness::Complete,
            &Default::default(),
        );
        let order = sorted_columns(&profile, ProfileSort::NullRate, true);
        let (cols, table) = summary_table(&profile, None, &order);
        assert_eq!(cols.len(), 10);
        assert_eq!(table[0][0], CellValue::String("amount".into()));
        assert!(histogram_bars(&profile.columns[1]).len() > 1);
        assert!(!histogram_bars(&profile.columns[2]).is_empty());
        assert!(coverage_text(Coverage::of(&profile, Some(50))).contains("50"));
    }

    #[test]
    fn partial_profiles_say_how_much_they_read() {
        let profile = profile_rows(
            &columns(),
            &rows(10),
            Completeness::Truncated { cap: 10 },
            &Default::default(),
        );
        let text = coverage_text(Coverage::of(&profile, Some(5000)));
        assert!(text.contains("10") && text.contains("5000"), "{text}");
    }
}
