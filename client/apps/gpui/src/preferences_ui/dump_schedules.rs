//! Preferences ▸ Workspace ▸ Scheduled dumps pane — built-in dumps, native
//! backups (pg_dump/mysqldump), and saved-query exports.

use super::*;

use sqlly_datatable::{CellValue as GridCell, Column, ColumnKind, GridData, SqllyDataTable};

use crate::dump_schedule::{Cadence, DumpSchedules, ScheduleKind, next_due};

/// Short task label for the schedule grid.
fn task_label(kind: &ScheduleKind) -> String {
    match kind {
        ScheduleKind::BuiltinDump => {
            crate::localization::text("settings.dump_schedules.option.sql_dump")
        }
        ScheduleKind::NativeBackup { .. } => {
            crate::localization::text("settings.dump_schedules.option.native_backup")
        }
        ScheduleKind::QueryExport {
            query_name, format, ..
        } => crate::localization::format(
            "settings.dump_schedules.message.export_query_name",
            &[
                ("query_name", &(query_name).to_string()),
                ("value", &(format.label()).to_string()),
            ],
        ),
    }
}

/// The Scheduled dumps pane's cached view of `dump-schedules.json`: the
/// store (or why it could not be read) and the grid built from it, valid for
/// one store generation. Rendering never touches the disk; the cache reloads
/// when a save bumps the generation (the watcher task re-renders the window
/// the moment that happens) and when the pane is opened again, so a file
/// the user fixed by hand is picked up.
#[derive(Default)]
pub(super) struct ScheduledDumpsCache {
    entry: Option<CachedSchedules>,
    /// Wakes the window on every store save; dropped with the window.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Option<gpui::Task<()>>,
}

struct CachedSchedules {
    generation: u64,
    loaded: Result<std::rc::Rc<DumpSchedules>, String>,
    grid: Option<Entity<SqllyDataTable>>,
}

impl PreferencesWindow {
    /// Forget the cached store so the next render re-reads the file (the
    /// pane was just opened).
    pub(super) fn invalidate_scheduled_dumps_cache(&self) {
        self.scheduled_dumps_cache.borrow_mut().entry = None;
    }

    /// The cached store for this render, reloading only when the store's
    /// generation moved or the cache was invalidated. Returns the schedules,
    /// the load error (an unreadable store is shown, not treated as "no
    /// schedules"), and the grid.
    fn scheduled_dumps_state(
        &self,
        cx: &mut Context<Self>,
    ) -> (
        std::rc::Rc<DumpSchedules>,
        Option<String>,
        Option<Entity<SqllyDataTable>>,
    ) {
        let mut cache = self.scheduled_dumps_cache.borrow_mut();
        // The browser build's store never changes, so it needs no watcher.
        #[cfg(not(target_arch = "wasm32"))]
        if cache.watch.is_none() {
            let mut changes = crate::dump_schedule::subscribe_store_changes();
            cache.watch = Some(cx.spawn(async move |this, cx| {
                while changes.changed().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        let generation = crate::dump_schedule::store_generation();
        let fresh = cache
            .entry
            .as_ref()
            .is_some_and(|entry| entry.generation == generation);
        if !fresh {
            let loaded = DumpSchedules::load_checked().map(std::rc::Rc::new);
            let grid = match &loaded {
                Ok(schedules) if !schedules.schedules.is_empty() => {
                    build_schedule_grid(schedules, cx)
                }
                _ => None,
            };
            cache.entry = Some(CachedSchedules {
                generation,
                loaded,
                grid,
            });
        }
        let entry = cache.entry.as_ref().expect("cache filled above");
        match &entry.loaded {
            Ok(schedules) => (schedules.clone(), None, entry.grid.clone()),
            Err(error) => (
                std::rc::Rc::new(DumpSchedules::default()),
                Some(error.clone()),
                None,
            ),
        }
    }

    pub(super) fn render_scheduled_dumps(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // An unreadable store is shown, not treated as "no schedules" — the
        // app also refuses to save over it until the user fixes the file.
        let (schedules, store_error, grid) = self.scheduled_dumps_state(cx);
        let store = crate::connection::ConnectionStore::global();

        let grid_element = if let Some(error) = store_error {
            div()
                .text_color(Theme::error())
                .whitespace_normal()
                .child(crate::localization::format(
                    "settings.dump_schedules.message.load_scheduled_tasks_failed",
                    &[("error", &(error).to_string())],
                ))
                .into_any_element()
        } else if schedules.schedules.is_empty() {
            div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "settings.dump_schedules.child.no_scheduled_tasks_yet",
                ))
                .into_any_element()
        } else if let Some(grid) = grid {
            div().child(grid).into_any_element()
        } else {
            div()
                .text_color(Theme::error())
                .child(crate::localization::text(
                    "settings.dump_schedules.child.could_not_build_schedule_grid",
                ))
                .into_any_element()
        };

        let action_rows: Vec<AnyElement> = schedules
            .schedules
            .iter()
            .map(|schedule| {
                let profile = store.get(schedule.profile_id);
                let connection = profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| {
                    crate::localization::text("settings.dump_schedules.computed.missing")
                });
                let label = crate::localization::format(
                    "settings.dump_schedules.message.connection",
                    &[
                        ("connection", &(connection).to_string()),
                        ("database", &(schedule.database).to_string()),
                        ("value_2", &(task_label(&schedule.kind)).to_string()),
                    ],
                );
                let schedule_id = schedule.id;
                // The sqlly-cli binary is desktop-only, so the copyable line
                // has no browser equivalent and the button stays disabled
                // there. The line is built on click (it reads the library
                // preference), not on every render.
                #[cfg(not(target_arch = "wasm32"))]
                let cli_source = profile.as_ref().map(|p| {
                    (
                        schedule.clone(),
                        p.name.clone(),
                        p.database_type.connection_kind(),
                    )
                });
                #[cfg(target_arch = "wasm32")]
                let cli_source: Option<()> = None;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.0)))
                    .py(px(Theme::scaled(4.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .child(self.match_text(&label)),
                    )
                    .child(
                        Button::new(format!("dump-run-{schedule_id}"))
                            .label(crate::localization::text(
                                "settings.dump_schedules.label.run_now",
                            ))
                            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                                crate::app::AppView::update_active_app_view(cx, |app, _, cx| {
                                    app.run_scheduled_dump_now(schedule_id, cx);
                                });
                            })
                            .into_any_element(),
                    )
                    .child(
                        Button::new(format!("dump-edit-{schedule_id}"))
                            .label(crate::localization::text(
                                "settings.dump_schedules.label.edit",
                            ))
                            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                                crate::app::AppView::update_active_app_view(
                                    cx,
                                    |app, window, cx| {
                                        app.edit_scheduled_dump(schedule_id, window, cx);
                                    },
                                );
                            })
                            .into_any_element(),
                    )
                    .child(
                        Button::new(format!("dump-cli-{schedule_id}"))
                            .label(crate::localization::text(
                                "settings.dump_schedules.label.copy_cli_command",
                            ))
                            .disabled(cli_source.is_none())
                            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                                #[cfg(not(target_arch = "wasm32"))]
                                if let Some((schedule, connection, kind)) = &cli_source {
                                    let line = crate::cli_command::schedule_command(
                                        &crate::cli_command::cli_path(),
                                        schedule,
                                        connection,
                                        &crate::preferences::SqlLibraryPreferences::load()
                                            .effective_root(),
                                        *kind,
                                    );
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(line));
                                    crate::app::AppView::update_active_app_view(
                                        cx,
                                        |app, _, cx| app.note_schedule_cli_copied(cx),
                                    );
                                }
                                #[cfg(target_arch = "wasm32")]
                                let _ = (&cli_source, cx);
                            })
                            .into_any_element(),
                    )
                    .child(
                        Button::new(format!("dump-remove-{schedule_id}"))
                            .label(crate::localization::text(
                                "settings.dump_schedules.label.remove",
                            ))
                            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                                crate::app::AppView::update_active_app_view(cx, |app, _, cx| {
                                    app.remove_scheduled_dump(schedule_id, cx);
                                });
                            })
                            .into_any_element(),
                    )
                    .into_any_element()
            })
            .collect();

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.dump_schedules.heading.scheduled_dumps",
            )),
            self.intro(&crate::localization::text(
                "settings.dump_schedules.intro.intro",
            )),
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.0)))
                .child(
                    Button::new("dump-schedule-add")
                        .label(crate::localization::text(
                            "settings.dump_schedules.label.add",
                        ))
                        .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                            window.dispatch_action(Box::new(crate::app::AddScheduledDump), cx);
                        })
                        .into_any_element(),
                )
                .into_any_element(),
            div()
                .mt(px(Theme::scaled(8.0)))
                .min_h(px(Theme::scaled(160.0)))
                .child(grid_element)
                .into_any_element(),
            div()
                .mt(px(Theme::scaled(8.0)))
                .flex()
                .flex_col()
                .gap(px(Theme::scaled(4.0)))
                .children(action_rows)
                .into_any_element(),
        ])
    }
}

/// The schedule grid for `schedules`; `None` when the grid data is invalid.
/// Built once per store generation, not per render.
fn build_schedule_grid(
    schedules: &DumpSchedules,
    cx: &mut Context<PreferencesWindow>,
) -> Option<Entity<SqllyDataTable>> {
    let store = crate::connection::ConnectionStore::global();
    let now = chrono::Utc::now();
    let columns = vec![
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.connection"),
            kind: ColumnKind::Text,
            width: 140.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.database"),
            kind: ColumnKind::Text,
            width: 100.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.task"),
            kind: ColumnKind::Text,
            width: 150.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.cadence"),
            kind: ColumnKind::Text,
            width: 80.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.keep"),
            kind: ColumnKind::Integer,
            width: 60.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.next_run"),
            kind: ColumnKind::Text,
            width: 140.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.last_result"),
            kind: ColumnKind::Text,
            width: 200.0,
        },
        Column {
            name: crate::localization::text("settings.dump_schedules.computed.enabled"),
            kind: ColumnKind::Boolean,
            width: 70.0,
        },
    ];

    let rows: Vec<Vec<GridCell>> = schedules
        .schedules
        .iter()
        .map(|schedule| {
            let connection = store
                .get(schedule.profile_id)
                .map(|p| p.name)
                .unwrap_or_else(|| {
                    crate::localization::text("settings.dump_schedules.computed.missing_2")
                });
            let cadence = match schedule.cadence {
                Cadence::Hourly => {
                    crate::localization::text("settings.dump_schedules.option.hourly")
                }
                Cadence::Daily => crate::localization::text("settings.dump_schedules.option.daily"),
                Cadence::Weekly => {
                    crate::localization::text("settings.dump_schedules.option.weekly")
                }
            };
            let next = next_due(schedule, now)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string();
            vec![
                GridCell::Text(connection),
                GridCell::Text(schedule.database.clone()),
                GridCell::Text(task_label(&schedule.kind)),
                GridCell::Text(cadence),
                GridCell::Integer(schedule.retention as i64),
                GridCell::Text(next),
                GridCell::Text(schedule.last_result.clone().unwrap_or_else(|| "—".into())),
                GridCell::Boolean(schedule.enabled),
            ]
        })
        .collect();

    let data = GridData::new(columns, rows).ok()?;
    let theme = crate::grid_bridge::build_grid_theme();
    let view = SqllyDataTable::builder(data)
        .config(sqlly_datatable::GridConfig::default())
        .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
        .theme(theme)
        .build(cx);
    Some(cx.new(|_cx| view))
}
