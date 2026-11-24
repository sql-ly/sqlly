//! KPI cells: a scorecard row of KPI tiles, each a value from the input
//! (an aggregate, a row count or one cell) compared with the previous
//! period, a baseline cell's output or a target, with direction-aware
//! status, thresholds and a sparkline. The math is the analytics module's;
//! a tile over a cut-short input withholds its status.

use std::sync::Arc;

use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_services::analytics::kpi::{
    Aggregate, Comparison, Direction, KpiStatus, SparklineSpec, ThresholdBasis, Thresholds,
    ValueSource, format_value,
};
use sqlly_services::analytics::time::TimeGrain;
use sqlly_services::notebook::cells::kpi::{
    KpiCellSpec, KpiTile, MAX_TILES, TileOutcome, evaluate as evaluate_tiles, output_rows,
};
use sqlly_services::notebook::cells::spec;
use sqlly_services::notebook::cells::{output_view, output_view_of};

use super::image::{CachedImage, report_theme};
use super::{
    VisualCellView, VisualInputs, choice_button, column_options, dim, editor_panel, editor_row,
    small_button, t, text_field,
};
use crate::theme::Theme;

#[derive(Default)]
pub struct KpiPart {
    sparklines: Vec<CachedImage>,
}

/// The cell's spec, with one tile summing the input's first numeric column
/// (or counting rows) while the user has set up none.
fn effective_spec(inputs: &VisualInputs) -> KpiCellSpec {
    let mut cell: KpiCellSpec = spec::read(&inputs.spec);
    if cell.tiles.is_empty() {
        let first = inputs
            .input_columns()
            .into_iter()
            .find(|c| sqlly_services::report::format::ColumnKind::infer(&c.data_type).is_numeric());
        let label = first
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| t("notebook.visual.kpi.rows_label"));
        cell.tiles
            .push(KpiTile::new(label, first.as_ref().map(|c| c.name.as_str())));
    }
    cell
}

fn outcomes(inputs: &VisualInputs) -> Result<(KpiCellSpec, Vec<TileOutcome>), DisplayMessage> {
    let (output, rows) = inputs
        .input_rows()
        .ok_or_else(|| super::chart::missing_input(inputs))?;
    let cell = effective_spec(inputs);
    let view = output_view_of(&output, &rows);
    let baseline = cell
        .baseline
        .as_deref()
        .and_then(|b| inputs.others.iter().find(|o| o.id == b))
        .and_then(|o| o.output.clone());
    let baseline_view = baseline.as_ref().map(|b| output_view(b));
    let outcomes = evaluate_tiles(&cell, &view, baseline_view.as_ref());
    Ok((cell, outcomes))
}

pub(crate) fn evaluate(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), DisplayMessage> {
    let (_, outcomes) = outcomes(inputs)?;
    Ok(output_rows(&outcomes))
}

fn status_color(status: KpiStatus) -> gpui::Hsla {
    match status {
        KpiStatus::Good => Theme::success(),
        KpiStatus::Warning => Theme::warning(),
        KpiStatus::Critical => Theme::error(),
        KpiStatus::Neutral | KpiStatus::Unknown => Theme::text_dim(),
    }
}

fn status_key(status: KpiStatus) -> &'static str {
    match status {
        KpiStatus::Good => "notebook.visual.kpi.status.good",
        KpiStatus::Warning => "notebook.visual.kpi.status.warning",
        KpiStatus::Critical => "notebook.visual.kpi.status.critical",
        KpiStatus::Neutral => "notebook.visual.kpi.status.neutral",
        KpiStatus::Unknown => "notebook.visual.kpi.status.unknown",
    }
}

fn comparison_key(comparison: &Option<Comparison>) -> &'static str {
    match comparison {
        None => "notebook.visual.kpi.comparison.none",
        Some(Comparison::PreviousPeriod { .. }) => "notebook.visual.kpi.comparison.previous_period",
        Some(Comparison::Baseline) => "notebook.visual.kpi.comparison.baseline",
        Some(Comparison::Target { .. }) => "notebook.visual.kpi.comparison.target",
    }
}

impl VisualCellView {
    fn kpi_spec(&self) -> KpiCellSpec {
        effective_spec(&self.inputs)
    }

    fn update_kpi(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut KpiCellSpec)) {
        let mut cell = self.kpi_spec();
        change(&mut cell);
        cell.sync_inputs();
        self.emit_spec(spec::write(&cell), cx);
    }

    fn update_tile(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut KpiTile),
    ) {
        self.update_kpi(cx, |cell| {
            if let Some(tile) = cell.tiles.get_mut(index) {
                change(tile);
            }
        });
    }

    pub(crate) fn render_kpi(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let mut root = div().flex().flex_col().w_full();
        let cell = self.kpi_spec();
        if self.editing {
            root = root.child(self.render_kpi_editor(&cell, window, cx));
        }
        let result = outcomes(&self.inputs);
        let Ok((cell, outcomes)) = result else {
            return root
                .child(
                    div()
                        .h(px(s(60.0)))
                        .px(px(s(12.0)))
                        .flex()
                        .items_center()
                        .child(dim(t("notebook.visual.kpi.no_data"))),
                )
                .into_any_element();
        };
        let scale = window.scale_factor();
        let partial = self
            .inputs
            .input_rows()
            .is_some_and(|(output, _)| !output.completeness.supports_summaries());
        self.kpi
            .sparklines
            .resize_with(outcomes.len(), Default::default);
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(s(10.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)));
        for (index, outcome) in outcomes.iter().enumerate() {
            let tile = &cell.tiles[index];
            let mut card = div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .min_w(px(s(170.0)))
                .p(px(s(10.0)))
                .rounded(px(s(4.0)))
                .border_1()
                .border_color(Theme::border())
                .bg(Theme::bg())
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(tile.label.clone()),
                );
            match &outcome.result {
                Ok(result) => {
                    card = card.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(1.8)))
                            .text_color(Theme::text())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(format_value(result.value, &result.format)),
                    );
                    if let Some(delta) = result.delta_percent.or(result.delta_absolute) {
                        let favorable = result.favorable;
                        let color = match favorable {
                            Some(true) => Theme::success(),
                            Some(false) => Theme::error(),
                            None => Theme::text_dim(),
                        };
                        let text = match result.delta_percent {
                            Some(p) => crate::localization::format(
                                "notebook.visual.kpi.delta_percent",
                                &[("delta", format!("{p:+.1}"))],
                            ),
                            None => crate::localization::format(
                                "notebook.visual.kpi.delta_absolute",
                                &[(
                                    "delta",
                                    format!(
                                        "{}{}",
                                        if delta >= 0.0 { "+" } else { "" },
                                        format_value(delta, &result.format)
                                    ),
                                )],
                            ),
                        };
                        let versus = result.comparison_value.map(|v| {
                            crate::localization::format(
                                "notebook.visual.kpi.versus",
                                &[
                                    (
                                        "comparison",
                                        t(comparison_key(&tile.kpi.comparison)).as_str(),
                                    ),
                                    ("value", format_value(v, &result.format).as_str()),
                                ],
                            )
                        });
                        card = card.child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(s(6.0)))
                                .text_size(px(Theme::font_size_ratio(0.82)))
                                .child(div().text_color(color).child(text))
                                .when_some(versus, |d, v| {
                                    d.child(div().text_color(Theme::text_dim()).child(v))
                                }),
                        );
                    }
                    card = card.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .text_color(status_color(result.status))
                            .child(if result.inconclusive {
                                t("notebook.visual.kpi.inconclusive")
                            } else {
                                t(status_key(result.status))
                            }),
                    );
                    if result.sparkline.len() > 1 {
                        let values: Vec<f64> = result.sparkline.iter().map(|p| p.value).collect();
                        let key = format!("{values:?}{partial}");
                        let image = self.kpi.sparklines[index].get(key, scale, || {
                            let draw = if partial {
                                sqlly_services::report::svg::partial_sparkline
                            } else {
                                sqlly_services::report::svg::sparkline
                            };
                            let scene = draw(&values, 150.0, 32.0, report_theme());
                            sqlly_services::report::svg::to_svg(&scene, "spark")
                        });
                        if let Some(image) = image {
                            card = card.child(
                                div()
                                    .h(px(s(32.0)))
                                    .w(px(s(150.0)))
                                    .child(img(Arc::clone(&image)).size_full()),
                            );
                        }
                    }
                }
                Err(error) => {
                    card = card.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::error())
                            .child(kpi_error_text(error)),
                    );
                }
            }
            row = row.child(card);
        }
        root.child(row).into_any_element()
    }

    fn render_kpi_editor(
        &mut self,
        cell: &KpiCellSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = self.inputs.cell_id.clone();
        let this = cx.entity();
        let columns = self.inputs.input_columns();
        let mut panel = editor_panel();
        let baseline_options: Vec<(String, Option<String>, bool)> = std::iter::once((
            t("notebook.visual.kpi.editor.no_baseline"),
            None,
            cell.baseline.is_none(),
        ))
        .chain(
            self.inputs
                .candidates
                .iter()
                .filter(|(c, _)| Some(c) != self.inputs.input.as_ref().map(|i| &i.id))
                .map(|(c, n)| {
                    (
                        n.clone(),
                        Some(c.clone()),
                        cell.baseline.as_deref() == Some(c.as_str()),
                    )
                }),
        )
        .collect();
        let baseline_label = cell
            .baseline
            .as_deref()
            .and_then(|b| self.inputs.candidates.iter().find(|(c, _)| c == b))
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| t("notebook.visual.kpi.editor.no_baseline"));
        panel = panel.child(editor_row(
            t("notebook.visual.kpi.editor.baseline"),
            choice_button(
                format!("nb-kpi-baseline-{id}"),
                baseline_label,
                baseline_options,
                this.clone(),
                |v, b, cx| v.update_kpi(cx, |c| c.baseline = b),
            ),
        ));
        for (index, tile) in cell.tiles.iter().enumerate() {
            let label = self.field(
                &format!("kpi.label.{id}.{index}"),
                &tile.label,
                "notebook.visual.kpi.editor.label_placeholder",
                window,
                cx,
                move |v, text, cx| {
                    let text = text.to_string();
                    v.update_tile(index, cx, |t| t.label = text);
                },
            );
            let (value_column, aggregate) = match &tile.kpi.value {
                ValueSource::Aggregate { column, aggregate } => {
                    (Some(column.clone()), Some(*aggregate))
                }
                ValueSource::Cell { column, .. } => (column.clone(), None),
                ValueSource::RowCount => (None, None),
            };
            let aggregates = [
                (Some(Aggregate::Sum), "notebook.visual.kpi.aggregates.sum"),
                (Some(Aggregate::Avg), "notebook.visual.kpi.aggregates.avg"),
                (Some(Aggregate::Min), "notebook.visual.kpi.aggregates.min"),
                (Some(Aggregate::Max), "notebook.visual.kpi.aggregates.max"),
                (
                    Some(Aggregate::Median),
                    "notebook.visual.kpi.aggregates.median",
                ),
                (
                    Some(Aggregate::Count),
                    "notebook.visual.kpi.aggregates.count",
                ),
                (
                    Some(Aggregate::CountDistinct),
                    "notebook.visual.kpi.aggregates.count_distinct",
                ),
                (Some(Aggregate::Last), "notebook.visual.kpi.aggregates.last"),
                (None, "notebook.visual.kpi.aggregates.first_row"),
            ];
            let agg_label = |a: Option<Aggregate>| {
                aggregates
                    .iter()
                    .find(|(x, _)| *x == a)
                    .map(|(_, k)| t(k))
                    .unwrap_or_default()
            };
            let mut top = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(s(6.0)))
                .child(text_field(&label, 150.0))
                .child(choice_button(
                    format!("nb-kpi-column-{id}-{index}"),
                    value_column
                        .clone()
                        .unwrap_or_else(|| t("notebook.visual.kpi.rows_label")),
                    column_options(
                        &columns,
                        value_column.as_deref(),
                        Some(t("notebook.visual.kpi.rows_label")),
                    ),
                    this.clone(),
                    move |v, column: Option<String>, cx| {
                        v.update_tile(index, cx, |t| {
                            t.kpi.value = match column {
                                Some(column) => ValueSource::Aggregate {
                                    column,
                                    aggregate: Aggregate::Sum,
                                },
                                None => ValueSource::RowCount,
                            }
                        })
                    },
                ));
            if let Some(column) = value_column.clone() {
                top = top.child(choice_button(
                    format!("nb-kpi-agg-{id}-{index}"),
                    agg_label(aggregate),
                    aggregates
                        .iter()
                        .map(|(a, _)| (agg_label(*a), *a, *a == aggregate))
                        .collect(),
                    this.clone(),
                    move |v, a, cx| {
                        let column = column.clone();
                        v.update_tile(index, cx, |t| {
                            t.kpi.value = match a {
                                Some(aggregate) => ValueSource::Aggregate { column, aggregate },
                                None => ValueSource::Cell {
                                    column: Some(column),
                                    row: 0,
                                },
                            }
                        })
                    },
                ));
            }
            let time_column = columns
                .iter()
                .find(|c| {
                    sqlly_services::report::format::ColumnKind::infer(&c.data_type).is_temporal()
                })
                .map(|c| c.name.clone());
            let comparisons: Vec<(String, Option<Comparison>, bool)> = [
                None,
                time_column
                    .clone()
                    .map(|time_column| Comparison::PreviousPeriod {
                        time_column,
                        grain: Some(TimeGrain::Month),
                    }),
                Some(Comparison::Baseline),
                Some(Comparison::Target { value: 0.0 }),
            ]
            .into_iter()
            .enumerate()
            .filter(|(i, c)| *i == 0 || c.is_some())
            .map(|(_, c)| {
                let checked = std::mem::discriminant(&c)
                    == std::mem::discriminant(&tile.kpi.comparison)
                    && c.as_ref().map(std::mem::discriminant)
                        == tile.kpi.comparison.as_ref().map(std::mem::discriminant);
                (t(comparison_key(&c)), c, checked)
            })
            .collect();
            top = top.child(choice_button(
                format!("nb-kpi-compare-{id}-{index}"),
                t(comparison_key(&tile.kpi.comparison)),
                comparisons,
                this.clone(),
                move |v, c, cx| v.update_tile(index, cx, |t| t.kpi.comparison = c),
            ));
            if let Some(Comparison::Target { value }) = &tile.kpi.comparison {
                let target = self.field(
                    &format!("kpi.target.{id}.{index}"),
                    &value.to_string(),
                    "notebook.visual.kpi.editor.target_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        if let Ok(value) = text.trim().parse::<f64>() {
                            v.update_tile(index, cx, |t| {
                                t.kpi.comparison = Some(Comparison::Target { value })
                            });
                        }
                    },
                );
                top = top.child(text_field(&target, 80.0));
            }
            if let Some(Comparison::PreviousPeriod { time_column, grain }) = &tile.kpi.comparison {
                let grains = [
                    (TimeGrain::Day, "notebook.visual.chart.grains.day"),
                    (TimeGrain::Week, "notebook.visual.chart.grains.week"),
                    (TimeGrain::Month, "notebook.visual.chart.grains.month"),
                    (TimeGrain::Quarter, "notebook.visual.chart.grains.quarter"),
                    (TimeGrain::Year, "notebook.visual.chart.grains.year"),
                ];
                let current = grain.unwrap_or(TimeGrain::Month);
                let grain_label = |g: TimeGrain| {
                    grains
                        .iter()
                        .find(|(x, _)| *x == g)
                        .map(|(_, k)| t(k))
                        .unwrap_or_default()
                };
                let tc = time_column.clone();
                top = top.child(choice_button(
                    format!("nb-kpi-grain-{id}-{index}"),
                    grain_label(current),
                    grains
                        .iter()
                        .map(|(g, _)| (grain_label(*g), *g, *g == current))
                        .collect(),
                    this.clone(),
                    move |v, g, cx| {
                        let time_column = tc.clone();
                        v.update_tile(index, cx, |t| {
                            t.kpi.comparison = Some(Comparison::PreviousPeriod {
                                time_column,
                                grain: Some(g),
                            })
                        });
                    },
                ));
            }
            let directions = [
                (
                    Direction::HigherIsBetter,
                    "notebook.visual.kpi.direction.higher_is_better",
                ),
                (
                    Direction::LowerIsBetter,
                    "notebook.visual.kpi.direction.lower_is_better",
                ),
                (Direction::Neutral, "notebook.visual.kpi.direction.neutral"),
            ];
            let direction_label = |d: Direction| {
                directions
                    .iter()
                    .find(|(x, _)| *x == d)
                    .map(|(_, k)| t(k))
                    .unwrap_or_default()
            };
            top = top.child(choice_button(
                format!("nb-kpi-direction-{id}-{index}"),
                direction_label(tile.kpi.direction),
                directions
                    .iter()
                    .map(|(d, _)| (direction_label(*d), *d, *d == tile.kpi.direction))
                    .collect(),
                this.clone(),
                move |v, d, cx| v.update_tile(index, cx, |t| t.kpi.direction = d),
            ));
            let thresholds = tile.kpi.thresholds.unwrap_or_default();
            let warn = self.field(
                &format!("kpi.warn.{id}.{index}"),
                &thresholds.warn.map(|w| w.to_string()).unwrap_or_default(),
                "notebook.visual.kpi.editor.warn_placeholder",
                window,
                cx,
                move |v, text, cx| {
                    let value = text.trim().parse::<f64>().ok();
                    v.update_tile(index, cx, |t| {
                        let mut th = t.kpi.thresholds.unwrap_or(Thresholds {
                            basis: ThresholdBasis::DeltaPercent,
                            ..Thresholds::default()
                        });
                        th.warn = value;
                        t.kpi.thresholds =
                            (th.warn.is_some() || th.critical.is_some()).then_some(th);
                    });
                },
            );
            let critical = self.field(
                &format!("kpi.critical.{id}.{index}"),
                &thresholds
                    .critical
                    .map(|w| w.to_string())
                    .unwrap_or_default(),
                "notebook.visual.kpi.editor.critical_placeholder",
                window,
                cx,
                move |v, text, cx| {
                    let value = text.trim().parse::<f64>().ok();
                    v.update_tile(index, cx, |t| {
                        let mut th = t.kpi.thresholds.unwrap_or(Thresholds {
                            basis: ThresholdBasis::DeltaPercent,
                            ..Thresholds::default()
                        });
                        th.critical = value;
                        t.kpi.thresholds =
                            (th.warn.is_some() || th.critical.is_some()).then_some(th);
                    });
                },
            );
            let bases = [
                (ThresholdBasis::Value, "notebook.visual.kpi.basis.level"),
                (
                    ThresholdBasis::DeltaAbsolute,
                    "notebook.visual.kpi.basis.delta_absolute",
                ),
                (
                    ThresholdBasis::DeltaPercent,
                    "notebook.visual.kpi.basis.delta_percent",
                ),
            ];
            let basis_label = |b: ThresholdBasis| {
                bases
                    .iter()
                    .find(|(x, _)| *x == b)
                    .map(|(_, k)| t(k))
                    .unwrap_or_default()
            };
            let sparkline_on = tile.kpi.sparkline.is_some();
            let spark_time = time_column.clone();
            let bottom = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(s(6.0)))
                .child(dim(t("notebook.visual.kpi.editor.thresholds")))
                .child(choice_button(
                    format!("nb-kpi-basis-{id}-{index}"),
                    basis_label(thresholds.basis),
                    bases
                        .iter()
                        .map(|(b, _)| (basis_label(*b), *b, *b == thresholds.basis))
                        .collect(),
                    this.clone(),
                    move |v, basis, cx| {
                        v.update_tile(index, cx, |t| {
                            let mut th = t.kpi.thresholds.unwrap_or_default();
                            th.basis = basis;
                            t.kpi.thresholds = Some(th);
                        })
                    },
                ))
                .child(text_field(&warn, 70.0))
                .child(text_field(&critical, 70.0))
                .when_some(spark_time, |d, time_column| {
                    d.child(
                        small_button(
                            format!("nb-kpi-spark-{id}-{index}"),
                            t("notebook.visual.kpi.editor.sparkline"),
                        )
                        .when(sparkline_on, |b| {
                            use gpui_component::button::ButtonVariants as _;
                            b.primary()
                        })
                        .on_click(cx.listener(move |v, _, _, cx| {
                            let time_column = time_column.clone();
                            v.update_tile(index, cx, |t| {
                                t.kpi.sparkline = if sparkline_on {
                                    None
                                } else {
                                    Some(SparklineSpec {
                                        time_column,
                                        value_column: None,
                                        aggregate: Aggregate::Sum,
                                        grain: None,
                                        max_points: 60,
                                    })
                                }
                            })
                        })),
                    )
                })
                .child(
                    small_button(
                        format!("nb-kpi-remove-{id}-{index}"),
                        t("notebook.visual.kpi.editor.remove"),
                    )
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.drop_fields("kpi.");
                        v.update_kpi(cx, |c| {
                            if index < c.tiles.len() {
                                c.tiles.remove(index);
                            }
                        });
                    })),
                );
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(3.0)))
                    .pb(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(top)
                    .child(bottom),
            );
        }
        if cell.tiles.len() < MAX_TILES {
            let first = columns.first().map(|c| c.name.clone());
            panel = panel.child(
                small_button(
                    format!("nb-kpi-add-{id}"),
                    t("notebook.visual.kpi.editor.add"),
                )
                .on_click(cx.listener(move |v, _, _, cx| {
                    let first = first.clone();
                    v.update_kpi(cx, |c| {
                        c.tiles.push(KpiTile::new(
                            t("notebook.visual.kpi.new_tile"),
                            first.as_deref(),
                        ))
                    });
                })),
            );
        }
        panel.into_any_element()
    }
}

fn kpi_error_text(error: &sqlly_services::analytics::kpi::KpiError) -> String {
    use sqlly_services::analytics::kpi::KpiError;
    match error {
        KpiError::MissingColumn { column } => crate::localization::format(
            "notebook.visual.kpi.errors.missing_column",
            &[("column_name", column.as_str())],
        ),
        _ => t("notebook.visual.kpi.errors.cannot_compute"),
    }
}
