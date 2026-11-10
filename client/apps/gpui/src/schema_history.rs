//! Schema snapshot UI: the small "Save Schema Snapshot" dialog that runs the
//! Schema Compare collector against one database and stores the result, and
//! the Schema History dialog — a timeline plus list of one database's
//! snapshots from which any two points (or a point and Live) open the
//! existing Schema Compare pane in its diff state.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, MouseButton, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_datatable::SqllyDataTable;

use crate::dialog::{Dialog, DialogFrame};
use crate::schema_compare::{collect_objects_sql, collection_limit_reached, objects_from_outcome};
use crate::schema_snapshots::{
    CompareSource, SchemaSnapshotMeta, SchemaSnapshotStore, TimelinePoint, compare_pair,
    format_taken_at, toggle_pick,
};
use crate::theme::Theme;
use crate::tool_dialog::{TOOL_QUERY_ROW_CAP, ToolDialogContext, ToolQuerySet};

// ── Save Schema Snapshot ───────────────────────────────────────────────────

pub enum SchemaSnapshotDialogEvent {
    Saved(SchemaSnapshotMeta),
    Closed,
}

pub struct SaveSchemaSnapshotDialog {
    context: ToolDialogContext,
    database: String,
    store: SchemaSnapshotStore,
    label_input: Entity<InputState>,
    collecting: bool,
    generation: u64,
    error: Option<String>,
    frame: DialogFrame,
    _subscription: Subscription,
}

impl EventEmitter<SchemaSnapshotDialogEvent> for SaveSchemaSnapshotDialog {}

impl SaveSchemaSnapshotDialog {
    /// `context.profile.database` is the database to capture.
    pub fn new(
        context: ToolDialogContext,
        store: SchemaSnapshotStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let database = context.profile.database.clone().unwrap_or_default();
        let label_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.schema_history.label_placeholder",
            ))
        });
        let subscription = cx.subscribe(
            &label_input,
            |this: &mut Self, _state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save(cx);
                }
            },
        );
        Self {
            context,
            database,
            store,
            label_input,
            collecting: false,
            generation: 0,
            error: None,
            frame: DialogFrame::new(Theme::scaled(480.0), Theme::scaled(270.0)),
            _subscription: subscription,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.collecting {
            return;
        }
        let profile = self.context.profile.clone();
        let database = self.database.clone();
        let sql = match collect_objects_sql(profile.database_type, &database) {
            Ok(sql) => sql,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.collecting = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.generation != generation {
                return;
            }
            this.collecting = false;
            if let Some(error) = outcome.error {
                this.error = Some(crate::localization::format(
                    "explorer.schema_history.read_failed",
                    &[("error", &error)],
                ));
                cx.notify();
                return;
            }
            if collection_limit_reached(&outcome) {
                this.error = Some(crate::localization::format(
                    "explorer.schema_history.collection_limit",
                    &[("cap", &TOOL_QUERY_ROW_CAP.to_string())],
                ));
                cx.notify();
                return;
            }
            let objects = objects_from_outcome(&outcome);
            let label = this.label_input.read(cx).value().to_string();
            let now_ms = chrono::Utc::now().timestamp_millis();
            match this
                .store
                .save_new(&profile, &database, Some(&label), now_ms, &objects)
            {
                Ok(meta) => {
                    cx.emit(SchemaSnapshotDialogEvent::Saved(meta));
                    cx.emit(SchemaSnapshotDialogEvent::Closed);
                }
                Err(error) => {
                    this.error = Some(crate::localization::format(
                        "explorer.schema_history.save_failed",
                        &[("error", crate::localization::message(error))],
                    ))
                }
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Cancel an in-flight read (its result is ignored) or close the dialog.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.collecting {
            self.generation += 1;
            self.collecting = false;
            cx.notify();
            return;
        }
        cx.emit(SchemaSnapshotDialogEvent::Closed);
    }
}

impl Render for SaveSchemaSnapshotDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let profile = &self.context.profile;
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .text_size(px(Theme::font_size()))
            .child(div().text_color(Theme::text_dim()).child(format!(
                "{} · {} · {}",
                profile.name,
                self.database,
                profile.database_type.display_name()
            )))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.schema_history.capture_explanation",
                    )),
            )
            .child(Input::new(&self.label_input).small());
        if self.collecting {
            body = body.child(div().text_color(Theme::text_dim()).child(
                crate::localization::format(
                    "explorer.schema_history.reading",
                    &[("database", &self.database)],
                ),
            ));
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::error())
                    .whitespace_normal()
                    .child(error.clone()),
            );
        }
        Dialog::new("save-schema-snapshot", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "explorer.schema_history.save_title",
            ))
            .easy_close(true)
            .on_close(|this, _window, cx| this.cancel(cx))
            .child(body)
            .button(
                Button::new("schema-snapshot-cancel")
                    .label(crate::localization::text(if self.collecting {
                        "explorer.schema_history.cancel_read_button"
                    } else {
                        "explorer.schema_history.cancel_button"
                    }))
                    .ghost()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.cancel(cx))),
            )
            .button(
                Button::new("schema-snapshot-save")
                    .primary()
                    .label(crate::localization::text(if self.collecting {
                        "explorer.schema_history.saving_button"
                    } else {
                        "explorer.schema_history.save_button"
                    }))
                    .disabled(self.collecting)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.save(cx))),
            )
            .build(self, window, cx)
    }
}

// ── Schema History ─────────────────────────────────────────────────────────

pub enum SchemaHistoryEvent {
    /// Open Schema Compare between the two picked points.
    Compare {
        source: Box<CompareSource>,
        target: Box<CompareSource>,
    },
    /// Open the Save Schema Snapshot dialog for this database.
    TakeSnapshot {
        profile: Box<ConnectionProfile>,
    },
    Closed,
}

pub struct SchemaHistoryDialog {
    profile: ConnectionProfile,
    database: String,
    store: SchemaSnapshotStore,
    /// Oldest first — the timeline's left-to-right order.
    chronological: Vec<SchemaSnapshotMeta>,
    picks: Vec<TimelinePoint>,
    grid: Option<Entity<SqllyDataTable>>,
    rename_input: Entity<InputState>,
    error: Option<String>,
    frame: DialogFrame,
    _subscription: Subscription,
}

impl EventEmitter<SchemaHistoryEvent> for SchemaHistoryDialog {}

/// The list shows newest first; the timeline oldest first. Map a list row
/// back to its chronological index.
fn chronological_index_for_row(row: usize, count: usize) -> Option<usize> {
    (row < count).then(|| count - 1 - row)
}

fn snapshot_grid_set(chronological: &[SchemaSnapshotMeta]) -> ToolQuerySet {
    let columns = [
        "explorer.schema_history.column_taken_at",
        "explorer.schema_history.column_label",
        "explorer.schema_history.column_objects",
        "explorer.schema_history.column_engine",
    ]
    .into_iter()
    .map(|name| ResultColumn {
        name: crate::localization::text(name),
        data_type: "nvarchar".to_string(),
        nullable: false,
    })
    .collect();
    let rows = chronological
        .iter()
        .rev()
        .map(|meta| {
            vec![
                CellValue::String(format_taken_at(meta.taken_unix_ms)),
                CellValue::String(meta.label.clone().unwrap_or_default()),
                CellValue::String(meta.object_count.to_string()),
                CellValue::String(meta.engine.display_name().to_string()),
            ]
        })
        .collect();
    ToolQuerySet { columns, rows }
}

impl SchemaHistoryDialog {
    /// `profile.database` names the database whose history is shown.
    pub fn new(
        profile: ConnectionProfile,
        store: SchemaSnapshotStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let database = profile.database.clone().unwrap_or_default();
        let rename_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.schema_history.rename_placeholder",
            ))
        });
        let subscription = cx.subscribe(
            &rename_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let value = state.read(cx).value().to_string();
                    this.apply_rename(value, cx);
                }
            },
        );
        let mut this = Self {
            profile,
            database,
            store,
            chronological: Vec::new(),
            picks: Vec::new(),
            grid: None,
            rename_input,
            error: None,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(520.0)),
            _subscription: subscription,
        };
        this.reload(cx);
        this
    }

    /// Re-read the store (after a snapshot was saved elsewhere, a rename, or
    /// a delete). Picks are dropped because indexes may have shifted.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let mut snapshots = self.store.list_for(self.profile.id.0, &self.database);
        snapshots.reverse();
        self.chronological = snapshots;
        self.picks.clear();
        self.grid =
            crate::tool_dialog::build_result_grid(&snapshot_grid_set(&self.chronological), cx);
        cx.notify();
    }

    fn live_source(&self) -> CompareSource {
        let mut profile = self.profile.clone();
        profile.database = Some(self.database.clone());
        CompareSource::Live {
            profile: Box::new(profile),
            database: self.database.clone(),
        }
    }

    fn selected_snapshot(&self, cx: &Context<Self>) -> Option<&SchemaSnapshotMeta> {
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        chronological_index_for_row(row, self.chronological.len())
            .and_then(|index| self.chronological.get(index))
    }

    fn apply_rename(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(id) = self.selected_snapshot(cx).map(|meta| meta.id.clone()) else {
            self.error = Some(crate::localization::text(
                "explorer.schema_history.rename_select_error",
            ));
            cx.notify();
            return;
        };
        match self.store.rename(&id, Some(&value)) {
            Ok(()) => {
                self.error = None;
                self.reload(cx);
            }
            Err(error) => self.error = Some(crate::localization::message(error)),
        }
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_snapshot(cx).map(|meta| meta.id.clone()) else {
            self.error = Some(crate::localization::text(
                "explorer.schema_history.delete_select_error",
            ));
            cx.notify();
            return;
        };
        match self.store.delete(&id) {
            Ok(()) => {
                self.error = None;
                self.reload(cx);
            }
            Err(error) => self.error = Some(crate::localization::message(error)),
        }
        cx.notify();
    }

    fn compare(&mut self, cx: &mut Context<Self>) {
        let live = self.live_source();
        let Some((source, target)) = compare_pair(&self.picks, &self.chronological, &live) else {
            self.error = Some(crate::localization::text(
                "explorer.schema_history.compare_pick_error",
            ));
            cx.notify();
            return;
        };
        cx.emit(SchemaHistoryEvent::Compare {
            source: Box::new(source),
            target: Box::new(target),
        });
        cx.emit(SchemaHistoryEvent::Closed);
    }

    fn take_snapshot(&mut self, cx: &mut Context<Self>) {
        let mut profile = self.profile.clone();
        profile.database = Some(self.database.clone());
        cx.emit(SchemaHistoryEvent::TakeSnapshot {
            profile: Box::new(profile),
        });
    }

    fn toggle(&mut self, point: TimelinePoint, cx: &mut Context<Self>) {
        toggle_pick(&mut self.picks, point);
        self.error = None;
        cx.notify();
    }

    fn timeline_node(
        &self,
        id: gpui::ElementId,
        point: TimelinePoint,
        title: String,
        when: String,
        detail: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let picked = self.picks.contains(&point);
        let live = point == TimelinePoint::Live;
        let dot = div()
            .size(px(s(14.0)))
            .rounded_full()
            .border_2()
            .border_color(if picked || live {
                Theme::accent()
            } else {
                Theme::border_hover()
            })
            .bg(if picked {
                Theme::accent()
            } else {
                Theme::panel_bg()
            });
        div()
            .id(id)
            .min_w(px(s(120.0)))
            .px(px(s(6.0)))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(s(4.0)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| this.toggle(point, cx)),
            )
            .child(dot)
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(if picked {
                        Theme::accent()
                    } else {
                        Theme::text()
                    })
                    .max_w(px(s(140.0)))
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(when),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(detail),
            )
    }
}

impl Render for SchemaHistoryDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let can_compare = self.picks.len() == 2;

        // Timeline: an axis line behind evenly spaced nodes, oldest on the
        // left, Live pinned at the right end.
        let mut nodes = div().flex().flex_row().items_start().relative().child(
            div()
                .absolute()
                .left(px(s(60.0)))
                .right(px(s(60.0)))
                .top(px(s(6.0)))
                .h(px(2.0))
                .bg(Theme::border_hover()),
        );
        for (index, meta) in self.chronological.iter().enumerate() {
            let title = meta
                .label
                .clone()
                .unwrap_or_else(|| crate::localization::text("explorer.schema_history.snapshot"));
            let node = self.timeline_node(
                ("schema-timeline-node", index).into(),
                TimelinePoint::Snapshot(index),
                title,
                format_taken_at(meta.taken_unix_ms),
                crate::localization::format(
                    "explorer.schema_history.object_count",
                    &[("count", &meta.object_count.to_string())],
                ),
                cx,
            );
            nodes = nodes.child(node);
        }
        let live_node = self.timeline_node(
            "schema-timeline-live".into(),
            TimelinePoint::Live,
            crate::localization::text("explorer.schema_history.live"),
            crate::localization::text("explorer.schema_history.now"),
            crate::localization::text("explorer.schema_history.current_schema"),
            cx,
        );
        nodes = nodes.child(live_node);

        let timeline = div()
            .id("schema-timeline")
            .w_full()
            .overflow_x_scroll()
            .px(px(s(6.0)))
            .py(px(s(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(nodes);

        let picked_summary = if can_compare {
            let live = self.live_source();
            compare_pair(&self.picks, &self.chronological, &live)
                .map(|(source, target)| {
                    crate::localization::format(
                        "explorer.schema_history.compare_summary",
                        &[
                            ("source", &source.display_name()),
                            ("target", &target.display_name()),
                        ],
                    )
                })
                .unwrap_or_default()
        } else {
            crate::localization::text("explorer.schema_history.compare_hint")
        };

        let mut body = div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(Theme::font_size()))
            .child(
                div()
                    .px(px(s(12.0)))
                    .pt(px(s(10.0)))
                    .pb(px(s(4.0)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_color(Theme::text())
                            .child(format!("{} · {}", self.profile.name, self.database)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                if self.chronological.len() == 1 {
                                    "explorer.schema_history.snapshot_count_one"
                                } else {
                                    "explorer.schema_history.snapshot_count_many"
                                },
                                &[("count", &self.chronological.len().to_string())],
                            )),
                    ),
            );
        if self.chronological.is_empty() {
            body = body.child(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::text("explorer.schema_history.empty")),
            );
        }
        body = body.child(timeline).child(
            div()
                .px(px(s(12.0)))
                .py(px(s(4.0)))
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(if can_compare {
                    Theme::accent()
                } else {
                    Theme::text_dim()
                })
                .whitespace_normal()
                .child(picked_summary),
        );
        if let Some(grid) = &self.grid {
            body = body.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .px(px(s(12.0)))
                    .pb(px(s(6.0)))
                    .child(grid.clone()),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .whitespace_normal()
                    .child(error.clone()),
            );
        }

        Dialog::new("schema-history", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.schema_history.title"))
            .easy_close(true)
            .bare_body()
            .on_close(|_this, _window, cx| cx.emit(SchemaHistoryEvent::Closed))
            .child(body)
            .leading_button(
                div()
                    .w(px(s(230.0)))
                    .child(Input::new(&self.rename_input).small()),
            )
            .leading_button(
                Button::new("schema-history-rename")
                    .label(crate::localization::text(
                        "explorer.schema_history.rename_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(self.chronological.is_empty())
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        let value = this.rename_input.read(cx).value().to_string();
                        this.apply_rename(value, cx);
                    })),
            )
            .leading_button(
                Button::new("schema-history-delete")
                    .label(crate::localization::text(
                        "explorer.schema_history.delete_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(self.chronological.is_empty())
                    .on_click(cx.listener(|this, _ev, _w, cx| this.delete_selected(cx))),
            )
            .button(
                Button::new("schema-history-take")
                    .label(crate::localization::text(
                        "explorer.schema_history.take_snapshot_button",
                    ))
                    .xsmall()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.take_snapshot(cx))),
            )
            .button(
                Button::new("schema-history-compare")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.schema_history.compare_button",
                    ))
                    .xsmall()
                    .disabled(!can_compare)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.compare(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    fn meta(taken: i64, label: Option<&str>, count: usize) -> SchemaSnapshotMeta {
        SchemaSnapshotMeta {
            id: format!("id-{taken}"),
            taken_unix_ms: taken,
            label: label.map(str::to_string),
            engine: DatabaseType::MySql,
            connection_id: uuid::Uuid::nil(),
            connection_name: "my".into(),
            database: "shop".into(),
            object_count: count,
            file: String::new(),
        }
    }

    #[test]
    fn list_rows_are_newest_first_and_map_back_to_the_timeline() {
        let chronological = vec![
            meta(100, Some("first"), 3),
            meta(200, None, 4),
            meta(300, Some("third"), 5),
        ];
        let set = snapshot_grid_set(&chronological);
        assert_eq!(set.columns.len(), 4);
        assert_eq!(set.rows.len(), 3);
        assert_eq!(set.rows[0][1], CellValue::String("third".into()));
        assert_eq!(set.rows[0][2], CellValue::String("5".into()));
        assert_eq!(set.rows[1][1], CellValue::String(String::new()));
        assert_eq!(set.rows[2][1], CellValue::String("first".into()));
        assert_eq!(set.rows[2][3], CellValue::String("MySQL".into()));
        assert_eq!(chronological_index_for_row(0, 3), Some(2));
        assert_eq!(chronological_index_for_row(2, 3), Some(0));
        assert_eq!(chronological_index_for_row(3, 3), None);
        assert_eq!(chronological_index_for_row(0, 0), None);
    }

    #[test]
    fn unlabeled_snapshots_still_have_a_title() {
        let unlabeled = meta(1, None, 0);
        assert!(crate::schema_snapshots::snapshot_title(&unlabeled).starts_with("Snapshot @ "));
    }
}
