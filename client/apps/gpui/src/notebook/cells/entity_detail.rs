//! Entity detail cells: one selected row (from a result or a parameter)
//! and the rows related to it through the foreign keys the user approved,
//! each in its own grid. Every lookup is a row-limited, read-only query.
//! A related set can become the master of a new detail cell below.

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::notebook::cells::entity_detail::{
    Direction, EntityDetailSpec, EntitySource, Relationship, TableName, catalog_relationships,
};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::analysis::ui::{self, body_text, caption, field, picker, t};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct EntityDetailCell;

impl NotebookCell for EntityDetailCell {
    fn kind(&self) -> &'static str {
        crate::notebook::analysis::KIND_ENTITY_DETAIL
    }

    fn label_key(&self) -> &'static str {
        "notebook.analysis.kind.entity_detail"
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

/// One relationship's rows after a run.
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedRows {
    /// Position in the spec's relationship list.
    pub index: usize,
    pub result: Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), sqlly_core::DisplayMessage>,
    /// More related rows exist than the limit.
    pub truncated: bool,
}

pub(crate) fn relationship_label(rel: &Relationship) -> String {
    let columns = rel
        .columns
        .iter()
        .map(|(a, b)| format!("{a} = {b}"))
        .collect::<Vec<_>>()
        .join(", ");
    crate::localization::format(
        match rel.direction {
            Direction::Outgoing => "notebook.analysis.entity.outgoing",
            Direction::Incoming => "notebook.analysis.entity.incoming",
        },
        &[
            ("table", rel.related_table().display()),
            ("columns", columns),
        ],
    )
}

/// `schema.name` (or `name`) typed by the user.
pub(crate) fn parse_table(text: &str) -> TableName {
    let text = text.trim();
    match text.rsplit_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => TableName {
            schema: Some(schema.trim().to_string()),
            name: name.trim().to_string(),
        },
        _ => TableName {
            schema: None,
            name: text.to_string(),
        },
    }
}

struct Shown {
    key_grid: Option<Entity<SqllyDataTable>>,
    related: Vec<(RelatedRows, Option<Entity<SqllyDataTable>>)>,
}

pub struct EntityDetailView {
    notebook: WeakEntity<NotebookView>,
    cell_id: String,
    shown: Option<Shown>,
    table_input: Option<Entity<InputState>>,
}

impl EntityDetailView {
    pub fn new(notebook: WeakEntity<NotebookView>, cell_id: String) -> Self {
        Self {
            notebook,
            cell_id,
            shown: None,
            table_input: None,
        }
    }

    #[cfg(test)]
    pub fn related(&self) -> Vec<RelatedRows> {
        self.shown
            .as_ref()
            .map(|s| s.related.iter().map(|(r, _)| r.clone()).collect())
            .unwrap_or_default()
    }

    pub fn set_result(
        &mut self,
        key: (Vec<ResultColumn>, Vec<CellValue>),
        related: Vec<RelatedRows>,
        cx: &mut Context<Self>,
    ) {
        let key_grid = ui::grid(&key.0, &[key.1], cx);
        let related = related
            .into_iter()
            .map(|r| {
                let grid = match &r.result {
                    Ok((cols, rows)) if !rows.is_empty() => ui::grid(cols, rows, cx),
                    _ => None,
                };
                (r, grid)
            })
            .collect();
        self.shown = Some(Shown { key_grid, related });
        cx.notify();
    }

    fn spec(&self, cx: &gpui::App) -> EntityDetailSpec {
        self.notebook
            .upgrade()
            .map(|nb| EntityDetailSpec::from_spec(&nb.read(cx).doc.spec(&self.cell_id)))
            .unwrap_or_default()
    }

    fn edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut EntityDetailSpec)) {
        Self::edit_at(&self.notebook, &self.cell_id, cx, change);
    }

    fn edit_view(
        view: &Entity<Self>,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut EntityDetailSpec),
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
        change: impl FnOnce(&mut EntityDetailSpec),
    ) {
        let Some(nb) = notebook.upgrade() else {
            return;
        };
        nb.update(cx, |nb, cx| {
            let mut spec = EntityDetailSpec::from_spec(&nb.doc.spec(id));
            change(&mut spec);
            nb.set_cell_spec(id, spec.to_spec(), cx);
        });
    }

    fn run(&self, cx: &mut gpui::App) {
        if let Some(nb) = self.notebook.upgrade() {
            let id = self.cell_id.clone();
            nb.update(cx, |nb, cx| nb.request_run_from_host(&id, cx));
        }
    }

    /// Step the selected row and run again.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.edit(cx, |s| {
            if let EntitySource::Row { row, .. } = &mut s.source {
                *row = row.saturating_add_signed(delta);
            }
        });
        self.run(cx);
    }

    /// Make relationship `index`'s rows the master of a new detail cell
    /// below this one.
    fn drill(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(shown) = &self.shown else {
            return;
        };
        let Some((rows, _)) = shown.related.iter().find(|(r, _)| r.index == index) else {
            return;
        };
        let Ok((columns, data)) = rows.result.clone() else {
            return;
        };
        let spec = self.spec(cx);
        let Some(rel) = spec.relationships.get(index).cloned() else {
            return;
        };
        let Some(nb) = self.notebook.upgrade() else {
            return;
        };
        let id = self.cell_id.clone();
        nb.update(cx, |nb, cx| {
            nb.analysis.outputs.insert(
                id.clone(),
                crate::notebook::analysis::inputs::derived_output(columns, data, "", 0, Vec::new()),
            );
            let at = nb.doc.index_of(&id).map_or(nb.doc.len(), |i| i + 1);
            let new_id = nb.insert_cell(crate::notebook::analysis::KIND_ENTITY_DETAIL, at, cx);
            let next = EntityDetailSpec {
                source: EntitySource::Row { cell: id, row: 0 },
                table: rel.related_table(),
                limit: spec.limit,
                ..EntityDetailSpec::default()
            };
            nb.set_cell_spec(&new_id, next.to_spec(), cx);
        });
    }

    fn candidates(&self, spec: &EntityDetailSpec, cx: &gpui::App) -> Vec<Relationship> {
        let Some(nb) = self.notebook.upgrade() else {
            return Vec::new();
        };
        let Some(catalog) = nb.read(cx).analysis.catalog.clone() else {
            return Vec::new();
        };
        catalog_relationships(
            &spec.table,
            catalog.objects.iter().map(|o| {
                (
                    o.schema.as_deref().unwrap_or_default(),
                    o.name.as_str(),
                    o.foreign_keys.as_slice(),
                )
            }),
        )
    }
}

impl Render for EntityDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let output_only =
            crate::notebook::present::cell_output_only(&self.notebook, &self.cell_id, cx);
        let spec = self.spec(cx);
        if self.table_input.is_none() {
            self.table_input = Some(ui::input(
                window,
                cx,
                "notebook.analysis.entity.table_placeholder",
                &spec.table.display(),
                |this: &mut Self, text, cx| {
                    let table = parse_table(&text);
                    this.edit(cx, |s| {
                        if s.table != table {
                            s.table = table;
                            s.relationships.clear();
                        }
                    });
                },
            ));
        }
        let (choices, params, running) = self
            .notebook
            .upgrade()
            .map(|nb| {
                let nb = nb.read(cx);
                (
                    nb.input_choices(&self.cell_id),
                    nb.doc
                        .params()
                        .into_iter()
                        .map(|(n, _)| n)
                        .collect::<Vec<_>>(),
                    nb.cell_state(&self.cell_id).is_active(),
                )
            })
            .unwrap_or_default();
        let mut sources: Vec<(String, String)> = choices
            .iter()
            .map(|(id, l)| (format!("cell:{id}"), l.clone()))
            .collect();
        for p in &params {
            sources.push((
                format!("param:{p}"),
                crate::localization::format(
                    "notebook.analysis.entity.source_parameter",
                    &[("name", p.as_str())],
                ),
            ));
        }
        let (source_value, row) = match &spec.source {
            EntitySource::Row { cell, row } => (format!("cell:{cell}"), Some(*row)),
            EntitySource::Parameter { name, .. } => (format!("param:{name}"), None),
        };
        let source_label = sources
            .iter()
            .find(|(v, _)| *v == source_value)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| t("notebook.analysis.input_choose"));
        let this = cx.weak_entity();
        let on_source: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |value, app| {
                if let Some(v) = this.upgrade() {
                    Self::edit_view(&v, app, |s| {
                        s.source = match value.strip_prefix("param:") {
                            Some(name) => EntitySource::Parameter {
                                name: name.to_string(),
                                column: s
                                    .relationships
                                    .first()
                                    .and_then(|r| r.columns.first())
                                    .map(|(c, _)| c.clone())
                                    .unwrap_or_default(),
                            },
                            None => EntitySource::Row {
                                cell: value.trim_start_matches("cell:").to_string(),
                                row: 0,
                            },
                        }
                    });
                }
            }
        });
        let candidates = self.candidates(&spec, cx);
        let approved: Vec<String> = spec.relationships.iter().map(Relationship::key).collect();
        let mut all = spec.relationships.clone();
        for c in &candidates {
            if !approved.contains(&c.key()) {
                all.push(c.clone());
            }
        }
        let rel_rows: Vec<AnyElement> = all
            .iter()
            .enumerate()
            .map(|(n, rel)| {
                let on = approved.contains(&rel.key());
                let rel = rel.clone();
                ui::checkbox(format!("nb-entity-rel-{n}"), relationship_label(&rel), on)
                    .on_click(cx.listener(move |this, _: &bool, _, cx| {
                        let rel = rel.clone();
                        this.edit(cx, |s| {
                            if let Some(at) =
                                s.relationships.iter().position(|r| r.key() == rel.key())
                            {
                                s.relationships.remove(at);
                            } else {
                                s.relationships.push(rel);
                            }
                        })
                    }))
                    .into_any_element()
            })
            .collect();
        let toolbar = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(8.0)))
            .child(field(
                t("notebook.analysis.entity.source_label"),
                picker(
                    "nb-entity-source",
                    source_label,
                    sources,
                    Some(source_value),
                    on_source,
                ),
            ))
            .children(self.table_input.clone().map(|i| {
                field(
                    t("notebook.analysis.entity.table_label"),
                    div().w(px(s(180.0))).child(Input::new(&i).small()),
                )
            }))
            .when_some(row, |d, row| {
                d.child(
                    ui::button("nb-entity-prev", t("notebook.analysis.entity.previous"))
                        .disabled(row == 0 || running)
                        .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
                )
                .child(caption(crate::localization::format(
                    "notebook.analysis.entity.row",
                    &[("index", (row + 1).to_string())],
                )))
                .child(
                    ui::button("nb-entity-next", t("notebook.analysis.entity.next"))
                        .disabled(running)
                        .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                )
            })
            .child(
                ui::primary_button("nb-entity-run", t("notebook.analysis.entity.run"))
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            );
        let mut root = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .w_full()
            .when(!output_only, |d| {
                d.child(toolbar)
                    .child(caption(crate::localization::format(
                        "notebook.analysis.entity.read_only_note",
                        &[("count", spec.limit().to_string())],
                    )))
                    .child(ui::section(t("notebook.analysis.entity.relationships")))
                    .when(all.is_empty(), |d| {
                        d.child(caption(t(
                            "notebook.analysis.entity.no_known_relationships",
                        )))
                    })
                    .child(div().flex().flex_col().children(rel_rows))
            });
        let Some(shown) = &self.shown else {
            return root;
        };
        root = root
            .child(ui::section(t("notebook.analysis.entity.selected_row")))
            .child(ui::grid_box(shown.key_grid.clone(), 56.0));
        for (rows, grid) in &shown.related {
            let Some(rel) = spec.relationships.get(rows.index) else {
                continue;
            };
            let index = rows.index;
            let count = rows.result.as_ref().map(|(_, r)| r.len()).unwrap_or(0);
            root = root.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(body_text(relationship_label(rel)))
                    .child(caption(crate::localization::format(
                        if rows.truncated {
                            "notebook.analysis.entity.rows_truncated"
                        } else if count == 1 {
                            "notebook.analysis.entity.rows_one"
                        } else {
                            "notebook.analysis.entity.rows"
                        },
                        &[("count", count.to_string())],
                    )))
                    .when(count > 0, |d| {
                        d.child(
                            ui::button(
                                format!("nb-entity-drill-{index}"),
                                t("notebook.analysis.entity.drill"),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.drill(index, cx))),
                        )
                    }),
            );
            root = match &rows.result {
                Err(e) => root.child(caption(crate::localization::message(e))),
                Ok(_) if grid.is_none() => root.child(caption(t("notebook.analysis.entity.none"))),
                Ok(_) => root.child(ui::grid_box(
                    grid.clone(),
                    (count as f32 * 24.0 + 32.0).clamp(60.0, 200.0),
                )),
            };
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_and_labels_read_naturally() {
        assert_eq!(
            parse_table("dbo.Orders"),
            TableName {
                schema: Some("dbo".into()),
                name: "Orders".into()
            }
        );
        assert_eq!(parse_table("Orders").schema, None);
        let rel = Relationship {
            name: String::new(),
            direction: Direction::Incoming,
            schema: Some("dbo".into()),
            table: "Orders".into(),
            columns: vec![("CustomerID".into(), "CustomerID".into())],
        };
        let label = relationship_label(&rel);
        assert!(
            label.contains("dbo.Orders") && label.contains("CustomerID"),
            "{label}"
        );
    }
}
