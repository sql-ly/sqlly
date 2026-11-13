use super::*;

/// Staged-edit marker column vs result-column indices, background column
/// scans, and the column-layout store's corrupt-file handling.
mod grid_columns;

/// The grid context menu clicked for real (right-click, then the row), and
/// where the dialogs it opens are painted.
mod grid_menu_dispatch;

#[test]
fn export_format_from_extension_round_trips() {
    for f in ExportFormat::all() {
        assert_eq!(
            ExportFormat::from_extension(f.extension()),
            Some(if f.extension() == "sql" {
                ExportFormat::Insert
            } else {
                *f
            }),
            "extension {} must resolve",
            f.extension()
        );
    }
}

#[test]
fn export_format_from_extension_is_case_insensitive_and_dot_tolerant() {
    assert_eq!(
        ExportFormat::from_extension(".XLSX"),
        Some(ExportFormat::Xlsx)
    );
    assert_eq!(
        ExportFormat::from_extension("Json"),
        Some(ExportFormat::Json)
    );
    assert_eq!(ExportFormat::from_extension("exe"), None);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn write_export_bytes_writes_a_temp_file() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqlly_write_export_ok_{}_{}.csv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_export_bytes(&path, b"a,b\n1,2\n", false).expect("write should succeed");
    let got = std::fs::read(&path).expect("read back");
    assert_eq!(got, b"a,b\n1,2\n");
    let _ = std::fs::remove_file(&path);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn write_export_bytes_errors_when_parent_is_a_file() {
    let mut parent = std::env::temp_dir();
    parent.push(format!(
        "sqlly_write_export_parent_{}_{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&parent, b"not a directory").expect("create parent file");
    let path = parent.join("results.csv");
    let err = write_export_bytes(&path, b"x", false).expect_err("parent is a file");
    assert!(
        err.contains(&path.display().to_string()),
        "error must include the path, got {err}"
    );
    let _ = std::fs::remove_file(&parent);
}

#[test]
fn spill_read_outcome_is_silent_when_generation_is_stale() {
    assert_eq!(spill_read_outcome(false, Some(&())), SpillReadUi::Silent);
    assert_eq!(spill_read_outcome(false, None::<&()>), SpillReadUi::Silent);
}

#[test]
fn spill_read_outcome_applies_when_generation_matches_and_rows_exist() {
    assert_eq!(spill_read_outcome(true, Some(&())), SpillReadUi::Apply);
}

#[test]
fn spill_read_outcome_empty_page_is_apply_not_error() {
    let empty: Vec<u8> = Vec::new();
    assert_eq!(spill_read_outcome(true, Some(&empty)), SpillReadUi::Apply);
}

#[test]
fn spill_read_outcome_reports_matching_generation_none() {
    assert_eq!(
        spill_read_outcome(true, None::<&()>),
        SpillReadUi::ReportError
    );
}
use crate::editor::DiagnosticSeverity;
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::SystemTime;
#[cfg(target_arch = "wasm32")]
use web_time::SystemTime;

/// gpui-component widgets read the library's global theme during render;
/// windowed tests must install it (idempotently) before opening windows.
fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
}

fn two_column_result_set() -> QueryEvent {
    QueryEvent::ResultSet {
        index: 0,
        columns: vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ],
    }
}

fn rows_batch(range: std::ops::Range<i64>) -> QueryEvent {
    QueryEvent::Rows {
        index: 0,
        rows: range
            .map(|i| vec![CellValue::I64(i), CellValue::String(format!("row{i}"))])
            .collect(),
    }
}

#[gpui::test]
fn single_column_profile_opens_the_profile_pane(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id, name FROM people", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_result_profile_for_column(0, 1, cx);
    });
    view.read_with(cx, |view, _cx| {
        assert!(view.show_result_profile);
        assert!(view.result_profile_grid.is_some());
        assert!(
            view.result_profile_caption.contains("name"),
            "caption names the profiled column: {}",
            view.result_profile_caption
        );
        // The detail pane's inputs are retained alongside the grid: the full
        // profile and the SQL the result came from.
        let profile = view.result_profile.as_ref().expect("profile retained");
        assert_eq!(profile.columns.len(), 1);
        assert_eq!(profile.columns[0].name, "name");
        assert_eq!(
            view.result_profile_sql.as_deref(),
            Some("SELECT id, name FROM people")
        );
    });
}

#[gpui::test]
fn profile_quick_filters_rebuild_the_grid_and_keep_the_column_mapping(
    cx: &mut gpui::TestAppContext,
) {
    use crate::result_profiler::ProfileFilter;
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        // id is numeric and unique; name is text and unique.
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_result_profile(0, cx);
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(view.result_profile_filter, ProfileFilter::All);
        assert_eq!(view.result_profile_indices, vec![0, 1]);
    });
    view.update(cx, |view, cx| {
        view.set_result_profile_filter(ProfileFilter::Numeric, cx);
    });
    view.read_with(cx, |view, _cx| {
        // Only the id column reads as numeric; the grid's single row must
        // map back to profiled column 0 for the detail pane.
        assert_eq!(view.result_profile_indices, vec![0]);
        assert!(view.result_profile_grid.is_some());
    });
    view.update(cx, |view, cx| {
        view.set_result_profile_filter(ProfileFilter::Constant, cx);
    });
    view.read_with(cx, |view, _cx| {
        // No constant columns: the grid stays present (headers only) and no
        // stale mapping survives.
        assert!(view.result_profile_indices.is_empty());
        assert!(view.result_profile_grid.is_some());
    });
}

#[gpui::test]
fn filter_builder_apply_sets_grid_filters(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_filter_builder(0, cx);
        if let Some(builder) = view.filter_builder.as_mut() {
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(1),
                    op: crate::grid_filter_builder::BuilderOp::Contains,
                    a: "row1".into(),
                    b: String::new(),
                    enabled: true,
                });
        }
        view.apply_filter_builder(cx);
    });
    view.read_with(cx, |view, cx| {
        let state = view.active_grid_state(cx).expect("grid state");
        let len = state.read(cx).display_indices.len();
        assert_eq!(
            len, 1,
            "only the row whose name contains row1 should remain"
        );
    });
}

#[gpui::test]
fn filter_builder_or_group_filters_client_side(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_filter_builder(0, cx);
        if let Some(builder) = view.filter_builder.as_mut() {
            builder.model.root.conjunction = crate::grid_filter_builder::Conjunction::Or;
            builder.model.clear_items();
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(1),
                    op: crate::grid_filter_builder::BuilderOp::Contains,
                    a: "row1".into(),
                    b: String::new(),
                    enabled: true,
                });
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(0),
                    op: crate::grid_filter_builder::BuilderOp::Eq,
                    a: "3".into(),
                    b: String::new(),
                    enabled: true,
                });
        }
        view.apply_filter_builder(cx);
    });
    view.read_with(cx, |view, cx| {
        let rows = view.visible_rows_for_slot(0, cx);
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![1, 3]);
    });
}

#[gpui::test]
fn client_filter_maps_source_rows_for_row_actions(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_filter_builder(0, cx);
        if let Some(builder) = view.filter_builder.as_mut() {
            builder.model.root.conjunction = crate::grid_filter_builder::Conjunction::Or;
            builder.model.clear_items();
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(1),
                    op: crate::grid_filter_builder::BuilderOp::Contains,
                    a: "row1".into(),
                    b: String::new(),
                    enabled: true,
                });
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(0),
                    op: crate::grid_filter_builder::BuilderOp::Eq,
                    a: "3".into(),
                    b: String::new(),
                    enabled: true,
                });
        }
        view.apply_filter_builder(cx);
        let row = view.source_row_for_slot(0, 1, cx).expect("mapped row");
        match row.first() {
            Some(CellValue::I64(3)) => {}
            other => panic!("expected id 3, got {other:?}"),
        }
    });
}

#[gpui::test]
fn clear_filters_drops_client_filter(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_filter_builder(0, cx);
        if let Some(builder) = view.filter_builder.as_mut() {
            builder.model.root.conjunction = crate::grid_filter_builder::Conjunction::Or;
            builder.model.clear_items();
            builder
                .model
                .push_row(crate::grid_filter_builder::FilterRow {
                    column: crate::grid_filter_builder::ColumnTarget::Index(1),
                    op: crate::grid_filter_builder::BuilderOp::Contains,
                    a: "row1".into(),
                    b: String::new(),
                    enabled: true,
                });
        }
        view.apply_filter_builder(cx);
        assert!(
            view.client_filter
                .first()
                .and_then(|f| f.as_ref())
                .is_some()
        );
        view.clear_slot_filters(0, cx);
        assert!(view.client_filter.iter().all(|f| f.is_none()));
        let rows = view.visible_rows_for_slot(0, cx);
        assert_eq!(rows.len(), 5);
    });
}

fn loaded_five_row_view(
    cx: &mut gpui::TestAppContext,
) -> (gpui::Entity<ResultsView>, &mut gpui::VisualTestContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    (view, cx)
}

fn visible_ids(view: &ResultsView, cx: &gpui::App) -> Vec<i64> {
    view.visible_rows_for_slot(0, cx)
        .iter()
        .filter_map(|row| match row.first() {
            Some(CellValue::I64(n)) => Some(*n),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn quick_filter_applies_immediately_and_stacks(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let mut row = FilterRow::new(0, BuilderOp::Ge);
        row.a = "2".into();
        view.apply_quick_filter(0, row, cx);
        assert_eq!(visible_ids(view, cx), vec![2, 3, 4]);
        // The builder surfaces holding exactly the quick condition.
        let builder = view.filter_builder.as_ref().expect("builder surfaced");
        assert_eq!(builder.model.leaf_count(), 1);
        let mut row = FilterRow::new(1, BuilderOp::IsNot);
        row.a = "row3".into();
        view.apply_quick_filter(0, row, cx);
        assert_eq!(visible_ids(view, cx), vec![2, 4]);
    });
}

#[gpui::test]
fn toggling_a_condition_off_refilters_live(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let mut row = FilterRow::new(1, BuilderOp::Contains);
        row.a = "row1".into();
        view.apply_quick_filter(0, row, cx);
        assert_eq!(visible_ids(view, cx), vec![1]);
        view.filter_builder_toggle_enabled(0, cx);
        assert_eq!(visible_ids(view, cx), vec![0, 1, 2, 3, 4]);
        assert!(!view.filter_builder.as_ref().unwrap().model.flat_rows()[0].enabled);
        view.filter_builder_toggle_enabled(0, cx);
        assert_eq!(visible_ids(view, cx), vec![1]);
    });
}

#[gpui::test]
fn reopening_the_builder_keeps_disabled_and_tree_conditions(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, Conjunction, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let mut row = FilterRow::new(1, BuilderOp::Contains);
        row.a = "row1".into();
        view.apply_quick_filter(0, row, cx);
        let mut paused = FilterRow::new(0, BuilderOp::Eq);
        paused.a = "4".into();
        paused.enabled = false;
        view.filter_builder.as_mut().unwrap().model.push_row(paused);
        view.apply_filter_builder(cx);
        view.close_filter_builder(cx);
        view.open_filter_builder(0, cx);
        let rows = view
            .filter_builder
            .as_ref()
            .unwrap()
            .model
            .flat_rows()
            .len();
        assert_eq!(rows, 2, "the paused condition survives close/reopen");

        // An OR tree also survives (it is not representable as grid filters).
        let builder = view.filter_builder.as_mut().unwrap();
        builder.model.root.conjunction = Conjunction::Or;
        builder.model.leaf_row_mut(1).unwrap().enabled = true;
        view.apply_filter_builder(cx);
        assert_eq!(visible_ids(view, cx), vec![1, 4]);
        view.close_filter_builder(cx);
        view.open_filter_builder(0, cx);
        let model = &view.filter_builder.as_ref().unwrap().model;
        assert_eq!(model.root.conjunction, Conjunction::Or);
        assert_eq!(model.leaf_count(), 2);

        // Clear Filters makes the remembered model stale.
        view.close_filter_builder(cx);
        view.clear_slot_filters(0, cx);
        view.open_filter_builder(0, cx);
        let model = &view.filter_builder.as_ref().unwrap().model;
        assert_eq!(model.leaf_count(), 1, "fresh builder gets one blank row");
        assert!(model.flat_rows()[0].a.is_empty());
    });
}

/// A default saved filter for table `dbo.people` on host-a/shop — `name`
/// contains `filter_value` — saved while the result was `SELECT *` (id,
/// name), applied through the view's test store (never the developer's
/// real filter file).
fn default_filter_store(filter_value: &str) -> crate::saved_filters::SavedFilterStore {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let mut model = crate::grid_filter_builder::FilterBuilderModel::default();
    let mut row = FilterRow::new(1, BuilderOp::Contains);
    row.a = filter_value.into();
    model.push_row(row);
    let saved = crate::saved_filters::SavedFilter {
        name: "small sets".into(),
        is_default: true,
        model,
        expression: None,
        columns: vec!["id".into(), "name".into()],
    };
    let mut store = crate::saved_filters::SavedFilterStore::default();
    store.upsert(
        &crate::saved_filters::filter_scope(&people_scope("host-a"), "SELECT * FROM dbo.people", 0)
            .key,
        saved,
    );
    store
}

fn people_scope(server: &str) -> crate::column_layout::LayoutScope {
    crate::column_layout::LayoutScope {
        server: server.into(),
        database: "shop".into(),
        default_schema: "dbo".into(),
        connection_id: "people-conn".into(),
        port: 1433,
        db_type: sqlly_lang::sqlmodel::DatabaseType::SqlServer,
    }
}

/// `SELECT name, id …`: the saved columns in the opposite order.
fn name_first_result_set() -> QueryEvent {
    QueryEvent::ResultSet {
        index: 0,
        columns: vec![
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
        ],
    }
}

fn name_first_rows(range: std::ops::Range<i64>) -> QueryEvent {
    QueryEvent::Rows {
        index: 0,
        rows: range
            .map(|i| vec![CellValue::String(format!("row{i}")), CellValue::I64(i)])
            .collect(),
    }
}

/// Ids of the visible rows, read from column `col`.
fn visible_ids_at(view: &ResultsView, cx: &gpui::App, col: usize) -> Vec<i64> {
    view.visible_rows_for_slot(0, cx)
        .iter()
        .filter_map(|row| match row.get(col) {
            Some(CellValue::I64(n)) => Some(*n),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn default_filter_follows_the_table_across_projections(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(default_filter_store("row1"));
        view.set_layout_scope(Some(people_scope("host-a")));
        // Same table as the saved scope, but a different projection — the
        // saved `name` condition was column 1 and is column 0 here — plus
        // a different order and predicate. The default must land on `name`
        // (applied by position it would test `id` and hide every row).
        view.set_query_sql(
            "SELECT name, id FROM dbo.people WHERE id > 0 ORDER BY name DESC",
            cx,
        );
        view.handle_event(name_first_result_set(), cx);
        view.handle_event(name_first_rows(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(visible_ids_at(view, cx, 1), vec![1]);
        assert!(
            view.filter_limitations.first().is_none_or(String::is_empty),
            "an applied default leaves no limitation caption"
        );
    });
}

#[gpui::test]
fn default_filter_on_a_missing_column_is_skipped_and_named(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(default_filter_store("row1"));
        view.set_layout_scope(Some(people_scope("host-a")));
        view.set_query_sql("SELECT id FROM dbo.people", cx);
        view.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..3).map(|i| vec![CellValue::I64(i)]).collect(),
            },
            cx,
        );
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(
            visible_ids(view, cx),
            vec![0, 1, 2],
            "the `name` condition is not applied to `id`"
        );
        let notice = view.filter_limitations.first().cloned().unwrap_or_default();
        assert!(
            notice.contains("small sets") && notice.contains("“name”"),
            "the skipped condition is named: {notice}"
        );
    });
}

#[gpui::test]
fn a_rebuild_does_not_reapply_the_default_over_the_users_filter(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(default_filter_store("row1"));
        view.set_layout_scope(Some(people_scope("host-a")));
        view.set_query_sql("SELECT * FROM dbo.people", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(visible_ids(view, cx), vec![1], "default applied once");

        // The user clears it; a grid rebuild (what a staged edit does) must
        // not bring it back.
        view.clear_slot_filters(0, cx);
        assert_eq!(visible_ids(view, cx), vec![0, 1, 2, 3, 4]);
        view.rebuild_slot_with_pending_markers(0, cx);
        assert_eq!(visible_ids(view, cx), vec![0, 1, 2, 3, 4]);

        // A filter the user applied survives the rebuild instead.
        let mut row = FilterRow::new(0, BuilderOp::Ge);
        row.a = "3".into();
        view.apply_quick_filter(0, row, cx);
        assert_eq!(visible_ids(view, cx), vec![3, 4]);
        view.rebuild_slot_with_pending_markers(0, cx);
        assert_eq!(visible_ids(view, cx), vec![3, 4]);
    });
}

#[gpui::test]
fn batches_never_auto_apply_a_default(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        let sql = "SELECT * FROM dbo.people; SELECT 1";
        let mut store = default_filter_store("row1");
        // Even a default stored under the batch's own scope is ignored.
        let batch_key = crate::saved_filters::filter_scope(&people_scope("host-a"), sql, 0).key;
        let saved = store.filters_for(
            &crate::saved_filters::filter_scope(
                &people_scope("host-a"),
                "SELECT * FROM dbo.people",
                0,
            )
            .key,
        )[0]
        .clone();
        store.upsert(&batch_key, saved);
        view.saved_filter_store_for_test = Some(store);
        view.set_layout_scope(Some(people_scope("host-a")));
        view.set_query_sql(sql, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(visible_ids(view, cx), vec![0, 1, 2, 3, 4]);
    });
}

#[gpui::test]
fn saving_a_filter_records_column_names_and_applies_by_name(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(Default::default());
        view.set_layout_scope(Some(people_scope("host-a")));
        view.set_query_sql("SELECT * FROM dbo.people", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let mut row = FilterRow::new(1, BuilderOp::Contains);
        row.a = "row4".into();
        view.apply_quick_filter(0, row, cx);
    });
    view.update_in(cx, |view, window, cx| {
        // The name input materializes at render time.
        let _ = view.render_filter_builder(window, cx);
        let input = view
            .filter_builder
            .as_ref()
            .and_then(|b| b.saved_name_input.clone())
            .expect("name input rendered");
        input.update(cx, |state, cx| state.set_value("fours", window, cx));
        view.filter_builder_save_named(cx);
        let store = view.saved_filter_store_for_test.clone().unwrap();
        let key = crate::saved_filters::filter_scope(
            &people_scope("host-a"),
            "SELECT * FROM dbo.people",
            0,
        )
        .key;
        let saved = store.get(&key, "fours").expect("saved");
        assert_eq!(saved.columns, vec!["id".to_string(), "name".to_string()]);
    });
}
#[gpui::test]
fn quick_filters_keep_header_value_checklists(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        // A header checklist on `name` allows rows 1, 2, and 3.
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.filters[1].values = Some(
                ["row1", "row2", "row3"]
                    .iter()
                    .map(|v| v.to_string())
                    .collect(),
            );
            s.recompute();
        });
        assert_eq!(visible_ids(view, cx), vec![1, 2, 3]);
        // A flat quick filter narrows within the checklist ...
        let mut row = FilterRow::new(0, BuilderOp::Ge);
        row.a = "2".into();
        view.apply_quick_filter(0, row, cx);
        assert_eq!(visible_ids(view, cx), vec![2, 3], "checklist kept");
        // ... and so does a tree filter.
        let builder = view.filter_builder.as_mut().unwrap();
        builder.model.root.conjunction = crate::grid_filter_builder::Conjunction::Or;
        let mut other = FilterRow::new(0, BuilderOp::Eq);
        other.a = "4".into();
        builder.model.push_row(other);
        view.apply_filter_builder(cx);
        assert_eq!(
            visible_ids(view, cx),
            vec![2, 3],
            "id >= 2 OR id = 4, still inside the checklist"
        );
        let values = state.read(cx).filters[1].values.clone();
        assert_eq!(values.map(|v| v.len()), Some(3));
    });
}

#[gpui::test]
fn quick_filter_on_an_or_tree_narrows_it(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, Conjunction, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let mut row = FilterRow::new(0, BuilderOp::Eq);
        row.a = "1".into();
        view.apply_quick_filter(0, row, cx);
        let builder = view.filter_builder.as_mut().unwrap();
        builder.model.root.conjunction = Conjunction::Or;
        let mut three = FilterRow::new(0, BuilderOp::Eq);
        three.a = "3".into();
        builder.model.push_row(three);
        view.apply_filter_builder(cx);
        assert_eq!(visible_ids(view, cx), vec![1, 3]);
        let mut not_three = FilterRow::new(1, BuilderOp::IsNot);
        not_three.a = "row3".into();
        view.apply_quick_filter(0, not_three, cx);
        assert_eq!(visible_ids(view, cx), vec![1], "narrowed, not widened");
    });
}

#[gpui::test]
fn quick_filter_on_a_windowed_result_says_why_it_did_nothing(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let spill_file = write_test_spill(5_000);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..100), cx);
        view.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 5_000,
            },
            cx,
        );
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        let mut row = FilterRow::new(0, BuilderOp::Eq);
        row.a = "1".into();
        view.apply_quick_filter(0, row, cx);
        let notice = view.filter_limitations.first().cloned().unwrap_or_default();
        assert!(notice.contains("Quick filters can't run"), "{notice}");
        assert!(view.filter_builder.is_none(), "no inert builder opens");
    });
}

#[gpui::test]
fn copying_from_a_grid_filtered_to_nothing_copies_nothing(cx: &mut gpui::TestAppContext) {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        assert_eq!(view.nothing_visible_to_copy(0, state.read(cx)), None);
        // Flat filter matching nothing.
        let mut row = FilterRow::new(0, BuilderOp::Eq);
        row.a = "99".into();
        view.apply_quick_filter(0, row, cx);
        let reason = view.nothing_visible_to_copy(0, state.read(cx));
        assert!(reason.is_some_and(|r| r.contains("hides every row")));
        // Tree filter matching nothing (rebuilds the grid from no rows).
        let builder = view.filter_builder.as_mut().unwrap();
        builder.model.root.conjunction = crate::grid_filter_builder::Conjunction::Or;
        view.apply_filter_builder(cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        assert!(view.nothing_visible_to_copy(0, state.read(cx)).is_some());
    });
}

#[gpui::test]
fn a_new_run_bumps_the_result_epoch(cx: &mut gpui::TestAppContext) {
    let (view, cx) = loaded_five_row_view(cx);
    view.update(cx, |view, cx| {
        let before = view.result_epoch();
        view.start_query(cx);
        assert_ne!(view.result_epoch(), before, "Top Values windows go inert");
    });
}

#[gpui::test]
fn builder_renders_doubly_nested_groups(cx: &mut gpui::TestAppContext) {
    let (view, cx) = loaded_five_row_view(cx);
    view.update_in(cx, |view, window, cx| {
        view.open_filter_builder(0, cx);
        use crate::grid_filter_builder::{
            BuilderOp, Conjunction, FilterBuilderModel, FilterGroup, FilterItem, FilterRow,
        };
        let leaf = |col: usize, op: BuilderOp, a: &str| {
            let mut row = FilterRow::new(col, op);
            row.a = a.into();
            FilterItem::Row(row)
        };
        // (name contains "row" AND (id = 1 OR id = 2)) OR id = 4
        let model = FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    FilterItem::Group(FilterGroup {
                        conjunction: Conjunction::And,
                        items: vec![
                            leaf(1, BuilderOp::Contains, "row"),
                            FilterItem::Group(FilterGroup {
                                conjunction: Conjunction::Or,
                                items: vec![
                                    leaf(0, BuilderOp::Eq, "1"),
                                    leaf(0, BuilderOp::Eq, "2"),
                                ],
                            }),
                        ],
                    }),
                    leaf(0, BuilderOp::Eq, "4"),
                ],
            },
        };
        view.filter_builder.as_mut().unwrap().model = model;
        assert!(view.render_filter_builder(window, cx).is_some());
        let builder = view.filter_builder.as_ref().unwrap();
        assert_eq!(
            builder.inputs.len(),
            builder.model.leaf_count(),
            "every leaf, at every depth, has its input row"
        );
        assert_eq!(
            builder.group_conjunction_selects.len(),
            2,
            "a conjunction dropdown for the nested and the doubly nested group"
        );
        // Deleting the last rendered row removes `id = 4`, not a hidden one.
        view.filter_builder_remove_row(3, cx);
        view.apply_filter_builder(cx);
        assert_eq!(visible_ids(view, cx), vec![1, 2]);
    });
}

#[gpui::test]
fn default_filters_do_not_cross_connections(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(default_filter_store("row1"));
        view.set_layout_scope(Some(people_scope("host-b")));
        view.set_query_sql("SELECT * FROM dbo.people", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(
            visible_ids(view, cx),
            vec![0, 1, 2, 3, 4],
            "the same SQL on another server finds no default"
        );
    });
}

#[gpui::test]
fn default_filter_skips_spill_backed_sets_with_a_visible_notice(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(5_000);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.saved_filter_store_for_test = Some(default_filter_store("row1"));
        view.set_layout_scope(Some(people_scope("host-a")));
        view.set_query_sql("SELECT * FROM dbo.people", cx);
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..100), cx);
        view.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 5_000,
            },
            cx,
        );
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert!(view.spill_backings[0].is_some(), "the backing is installed");
        let notice = view
            .filter_limitations
            .first()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(
            notice.contains("small sets") && notice.contains("too large"),
            "the skip is visible and names the filter: {notice}"
        );
        assert_eq!(
            view.grid_caption(0).unwrap_or_default(),
            notice,
            "the caption strip surfaces the limitation"
        );
        assert!(
            view.client_filter.first().is_none_or(Option::is_none),
            "no client filter is active on the windowed grid"
        );
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        let s = state.read(cx);
        assert_eq!(s.display_row_count(), 5_000, "windowed total intact");
    });
}

fn named_rows(pairs: &[(i64, &str)]) -> QueryEvent {
    QueryEvent::Rows {
        index: 0,
        rows: pairs
            .iter()
            .map(|(id, name)| vec![CellValue::I64(*id), CellValue::String((*name).to_string())])
            .collect(),
    }
}

#[gpui::test]
fn add_to_sort_orders_rows_by_secondary(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b"), (4, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.toggle_sort(1);
        });
        view.add_to_sort(0, 0, cx);
        view.add_to_sort(0, 0, cx);
    });
    view.read_with(cx, |view, cx| {
        let rows = view.visible_rows_for_slot(0, cx);
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![4, 2, 3, 1]);
    });
}

#[gpui::test]
fn keyboard_sort_command_uses_the_active_cell_column(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "c")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        grid.read(cx).state.clone().update(cx, |s, _| {
            s.selection = sqlly_datatable::Selection::Cell(0, 1);
        });
        view.sort_active_column(false, cx);
        assert_eq!(
            grid.read(cx).state.read(cx).sort,
            Some((1, sqlly_datatable::SortDirection::Ascending))
        );
        view.sort_active_column(true, cx);
        assert_eq!(
            grid.read(cx).state.read(cx).sort,
            Some((1, sqlly_datatable::SortDirection::Descending))
        );
    });
}

#[gpui::test]
fn keyboard_sort_command_keeps_secondary_sort_keys(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b"), (4, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.toggle_sort(1);
        });
        view.add_to_sort(0, 0, cx);
        view.add_to_sort(0, 0, cx);
        // A secondary key rebuilds the grid entity; select on the live one.
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        grid.read(cx).state.clone().update(cx, |s, _| {
            s.selection = sqlly_datatable::Selection::Cell(0, 1);
        });
        view.sort_active_column(true, cx);
        assert_eq!(view.sort_specs[0].keys.len(), 2);
    });
    view.read_with(cx, |view, cx| {
        let ids: Vec<i64> = view
            .visible_rows_for_slot(0, cx)
            .iter()
            .filter_map(|row| match row.first() {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![3, 1, 4, 2]);
    });
}

#[gpui::test]
fn keyboard_sort_command_ignores_row_selection(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        grid.read(cx).state.clone().update(cx, |s, _| {
            s.selection = sqlly_datatable::Selection::Row(0);
        });
        view.sort_active_column(false, cx);
        assert_eq!(grid.read(cx).state.read(cx).sort, None);
    });
}

#[gpui::test]
fn clear_sort_drops_secondaries(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b"), (4, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.toggle_sort(1);
        });
        view.add_to_sort(0, 0, cx);
        view.add_to_sort(0, 0, cx);
        assert!(
            view.sort_specs
                .first()
                .is_some_and(|spec| spec.keys.len() > 1)
        );
        view.clear_grid_sort(cx);
        assert!(view.sort_specs.iter().all(|spec| spec.keys.is_empty()));
    });
}

#[gpui::test]
fn sort_spec_survives_result_rebuild(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        view.set_layout_store_for_test(crate::column_layout::ColumnLayoutStore::default());
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b"), (4, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.toggle_sort(1);
        });
        view.add_to_sort(0, 0, cx);
        view.add_to_sort(0, 0, cx);
        let spec = view.sort_specs.first().cloned().expect("sort spec");
        let sig = crate::column_layout::signature_for(&view.result_sets[0].columns);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let state = grid.read(cx).state.read(cx);
        let layout = crate::column_layout::capture_from_grid(state, &sig, &spec, 0);
        let mut store = crate::column_layout::ColumnLayoutStore::default();
        store.upsert(key, layout);
        view.set_layout_store_for_test(store);
        view.clear(cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b"), (4, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert_eq!(
            view.sort_specs.first().map(|s| s.keys.len()),
            Some(spec.keys.len())
        );
        let rows = view.visible_rows_for_slot(0, cx);
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(CellValue::I64(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![4, 2, 3, 1]);
    });
}

#[gpui::test]
fn stale_grid_observer_survives_cleared_result_sets(cx: &mut gpui::TestAppContext) {
    // Repro for the detached-results hover crash: re-running a query clears
    // `result_sets` while the old grid (still rendered in the detached
    // window) keeps notifying its GridState observer with the stale slot.
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        view.set_layout_store_for_test(crate::column_layout::ColumnLayoutStore::default());
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(view.grid_views[0].is_some());
        // Schedule a layout save so flush_layout_save's generation check passes.
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        grid.read(cx).state.clone().update(cx, |s, _| {
            s.toggle_sort(0);
        });
        view.note_possible_layout_change(0, cx);
        let generation = view.layout_save_generation[0];
        // Simulate a new run having cleared the sets while the grid lives on.
        view.result_sets.clear();
        view.sort_specs.clear();
        view.note_possible_layout_change(0, cx);
        view.sync_sort_primary(0, cx);
        view.flush_layout_save(0, generation, cx);
    });
}

#[gpui::test]
fn saved_layout_is_applied_on_grid_build(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        let sig = crate::column_layout::signature_for(&[
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ]);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let mut store = crate::column_layout::ColumnLayoutStore::default();
        store.upsert(
            key,
            crate::column_layout::ColumnLayout {
                version: 1,
                signature: sig,
                widths: vec![321.0, 123.0],
                order: vec![1, 0],
                hidden: vec![],
                frozen: 1,
                sort: Default::default(),
                fk_labels: vec![],
            },
        );
        view.set_layout_store_for_test(store);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        let width = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[0]
            .width;
        assert_eq!(width, 321.0);
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx);
        assert_eq!(state.frozen_columns(), 1);
        assert_eq!(state.column_order(), &[1, 0]);
    });
}

#[gpui::test]
fn mismatched_signature_keeps_default_widths(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        let sig = crate::column_layout::signature_for(&[
            ResultColumn {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ]);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let mut store = crate::column_layout::ColumnLayoutStore::default();
        store.upsert(
            key,
            crate::column_layout::ColumnLayout {
                version: 1,
                signature: sig,
                widths: vec![321.0, 123.0],
                order: vec![0, 1],
                hidden: vec![],
                frozen: 0,
                sort: Default::default(),
                fk_labels: vec![],
            },
        );
        view.set_layout_store_for_test(store);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        let width = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[0]
            .width;
        // The mismatched saved width is ignored; the column is auto-sized to
        // its name and values ("id" over 1-character ids) instead.
        assert_ne!(width, 321.0);
        assert_eq!(
            width,
            crate::grid_bridge::auto_column_width(
                "id",
                1,
                crate::preferences::DEFAULT_AUTO_SIZE_MAX_CHARS
            )
        );
    });
}

#[gpui::test]
fn rebuild_preserves_dragged_width(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let grid = view.grid_views[0].as_ref().expect("grid");
        let state = grid.read(cx).state.clone();
        state.update(cx, |s, _| {
            s.data.columns[1].width = 500.0;
            s.sort = Some((0, sqlly_datatable::SortDirection::Descending));
            s.recompute();
        });
        view.synced_rows[0] = 0;
        view.handle_event(rows_batch(3..6), cx);
    });
    view.read_with(cx, |view, cx| {
        let width = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[1]
            .width;
        assert_eq!(width, 500.0);
    });
}

#[gpui::test]
fn reset_column_layout_restores_defaults(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        let sig = crate::column_layout::signature_for(&[
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ]);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let mut store = crate::column_layout::ColumnLayoutStore::default();
        store.upsert(
            key.clone(),
            crate::column_layout::ColumnLayout {
                version: 1,
                signature: sig,
                widths: vec![321.0, 123.0],
                order: vec![0, 1],
                hidden: vec![],
                frozen: 0,
                sort: Default::default(),
                fk_labels: vec![],
            },
        );
        view.set_layout_store_for_test(store);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.reset_column_layout(0, cx);
    });
    view.read_with(cx, |view, cx| {
        let width = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[0]
            .width;
        // The saved 321px is forgotten and the column is auto-sizing's
        // again: its name over one-character ids, not the saved width.
        assert_ne!(width, 321.0);
        assert_eq!(
            width,
            crate::grid_bridge::auto_column_width(
                "id",
                1,
                crate::preferences::DEFAULT_AUTO_SIZE_MAX_CHARS
            )
        );
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        assert!(
            view.layout_store
                .as_ref()
                .expect("store")
                .get(&key)
                .is_none()
        );
    });
}

#[test]
fn column_width_steps_stay_within_usable_bounds() {
    assert_eq!(super::adjusted_column_width(150.0, 40.0), 190.0);
    assert_eq!(super::adjusted_column_width(150.0, -40.0), 110.0);
    assert_eq!(super::adjusted_column_width(50.0, -40.0), 40.0);
    assert_eq!(super::adjusted_column_width(3_990.0, 40.0), 4_000.0);
}

#[gpui::test]
fn menu_width_adjustment_changes_and_remembers_the_column(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let before = view.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[0]
            .width;
        view.adjust_column_width(0, 0, 40.0, cx);
        let after = view.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .data
            .columns[0]
            .width;
        assert_eq!(after, before + 40.0);
        let generation = view.layout_save_generation[0];
        view.flush_layout_save(0, generation, cx);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let saved = view
            .layout_store
            .as_ref()
            .unwrap()
            .get(&key)
            .expect("width saved");
        assert_eq!(saved.widths[0], after);
        assert!(view.auto_size[&0].applied[0].is_none());
    });
}

#[gpui::test]
fn transpose_toggle_builds_and_clears(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.toggle_transpose(0, cx);
    });
    view.read_with(cx, |view, cx| {
        assert!(view.transpose_active.first().copied().unwrap_or(false));
        let cols = view
            .transposed
            .first()
            .and_then(|g| g.as_ref())
            .expect("transposed grid")
            .read(cx)
            .state
            .read(cx)
            .data
            .columns
            .len();
        assert_eq!(cols, 6, "Column + 5 source rows");
    });
    view.update(cx, |view, cx| view.toggle_transpose(0, cx));
    view.read_with(cx, |view, _cx| {
        assert!(!view.transpose_active.first().copied().unwrap_or(false));
        assert!(view.transposed.first().and_then(|g| g.as_ref()).is_none());
    });
}

#[gpui::test]
#[allow(clippy::type_complexity)]
fn follow_foreign_key_emits_event(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let events: std::rc::Rc<std::cell::RefCell<Vec<(usize, Vec<CellValue>, usize)>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::FollowForeignKeyRequested {
                result_set_index,
                row,
                column_index,
                ..
            } = event
            {
                seen.borrow_mut()
                    .push((*result_set_index, row.clone(), *column_index));
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.set_query_sql("SELECT id, name FROM t", cx);
        view.request_follow_foreign_key(0, 2, 1, cx);
    });
    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "one follow-FK request expected");
    let (result_set_index, row, column_index) = &recorded[0];
    assert_eq!(*result_set_index, 0);
    assert_eq!(*column_index, 1);
    assert_eq!(row[0], CellValue::I64(2));
}

#[gpui::test]
#[allow(clippy::type_complexity)]
fn cell_action_request_emits_event_with_source_row(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let events: std::rc::Rc<
        std::cell::RefCell<Vec<(Vec<CellValue>, String, crate::cell_actions::CellAction)>>,
    > = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::CellActionRequested {
                row,
                column_name,
                action,
                ..
            } = event
            {
                seen.borrow_mut()
                    .push((row.clone(), column_name.clone(), action.clone()));
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.set_query_sql("SELECT id, name FROM t", cx);
        view.request_cell_action(0, 2, 1, crate::cell_actions::CellAction::SetNull, cx);
    });
    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "one cell-action request expected");
    let (row, column_name, action) = &recorded[0];
    assert_eq!(*column_name, "name");
    assert_eq!(*action, crate::cell_actions::CellAction::SetNull);
    assert_eq!(row[0], CellValue::I64(2));
    assert_eq!(row[1], CellValue::String("row2".into()));
}

#[test]
fn detailed_diff_table_filters_by_class() {
    let baseline_columns = vec![
        ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        },
        ResultColumn {
            name: "v".into(),
            data_type: "int".into(),
            nullable: false,
        },
    ];
    let baseline = vec![
        vec![CellValue::I64(1), CellValue::I64(10)],
        vec![CellValue::I64(2), CellValue::I64(20)],
    ];
    let comparison = vec![
        vec![CellValue::I64(1), CellValue::I64(11)],
        vec![CellValue::I64(3), CellValue::I64(30)],
    ];
    let diff = result_set_difference::compare_detailed(
        &baseline_columns,
        &baseline,
        &baseline_columns,
        &comparison,
        &[0],
    )
    .unwrap();
    let (_, all) = detailed_diff_table(&diff, None);
    assert_eq!(all.len(), 3, "one added, one removed, one changed cell");
    let (_, added_only) = detailed_diff_table(&diff, Some("Added"));
    assert_eq!(added_only.len(), 1);
    assert!(matches!(&added_only[0][0], CellValue::String(s) if s == "Added"));
    let (_, changed_only) = detailed_diff_table(&diff, Some("Changed"));
    assert_eq!(changed_only.len(), 1);
    assert!(matches!(&changed_only[0][2], CellValue::String(s) if s == "v"));
}

#[gpui::test]
fn row_form_opens_steps_and_clamps(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_row_form(0, 1, cx);
    });
    view.read_with(cx, |view, _| {
        let form = view.row_form.as_ref().expect("form open");
        assert_eq!(form.row_index, 1);
        assert_eq!(form.row_count, 3);
        assert_eq!(form.columns.len(), 2);
        assert_eq!(form.cells.len(), 2);
    });
    view.update(cx, |view, cx| view.row_form_step(1, cx));
    view.read_with(cx, |view, _| {
        assert_eq!(view.row_form.as_ref().unwrap().row_index, 2);
    });
    // Stepping past the end is a no-op.
    view.update(cx, |view, cx| view.row_form_step(1, cx));
    view.read_with(cx, |view, _| {
        assert_eq!(view.row_form.as_ref().unwrap().row_index, 2);
    });
    view.update(cx, |view, cx| view.close_row_form(cx));
    view.read_with(cx, |view, _| assert!(view.row_form.is_none()));
}

#[gpui::test]
fn diagnostics_do_not_automatically_open_the_panel(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_diagnostics(
            vec![ValidationDiagnostic {
                severity: DiagnosticSeverity::Error,
                source: crate::editor::DiagnosticSource::Validation,
                message: "unknown column".into(),
                line: 1,
                identifier: None,
                start_byte: 0,
                end_byte: 0,
                explainer: None,
                fix: None,
            }],
            cx,
        );
    });

    view.read_with(cx, |view, _cx| {
        assert_eq!(view.diagnostics.len(), 1);
        assert!(
            !view.show_messages,
            "receiving diagnostics must not steal focus onto the Messages tab"
        );
    });
}

#[gpui::test]
#[cfg(not(target_arch = "wasm32"))]
fn historical_results_install_banner_and_dismiss_clears(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_historical_results(
            "Viewing results from 14:32:07 — Run to refresh".to_string(),
            vec![crate::query_version_history::SnapshotSet {
                columns: vec![ResultColumn {
                    name: "a".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
                rows: vec![vec![CellValue::I64(1)]],
                rows_affected: None,
            }],
            cx,
        );
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(
            view.historical_banner(),
            Some("Viewing results from 14:32:07 — Run to refresh")
        );
        assert_eq!(view.result_sets.len(), 1);
        assert_eq!(view.result_sets[0].rows.len(), 1);
    });
    view.update(cx, |view, cx| view.clear_historical(cx));
    view.read_with(cx, |view, _cx| {
        assert!(view.historical_banner().is_none());
        assert!(view.result_sets.is_empty());
    });
}

#[gpui::test]
#[cfg(not(target_arch = "wasm32"))]
fn start_query_clears_historical_banner(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_historical_results(
            "Viewing results from 14:32:07 — Run to refresh".to_string(),
            vec![],
            cx,
        );
        view.start_query(cx);
    });
    view.read_with(cx, |view, _cx| {
        assert!(view.historical_banner().is_none());
    });
}

const PLAN_SCAN_XML: &str = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="2"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" /></RelOp></StmtSimple></ShowPlanXML>"#;

fn sample_execution_plan() -> sqlly_services::plans::ExecutionPlan {
    sqlly_services::plans::parser::PlanParser::new()
        .parse_xml(PLAN_SCAN_XML)
        .unwrap()
}

#[gpui::test]
fn execution_plan_capture_lifecycle(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));

    view.update(cx, |view, cx| {
        view.show_messages = true;
        view.begin_execution_plan_capture(crate::execution_plan_view::PlanKind::Estimated, cx);
    });
    view.read_with(cx, |view, cx| {
        assert!(view.show_execution_plan, "capture brings the pane forward");
        assert!(!view.show_messages, "capture wins the body over messages");
        assert!(view.is_capturing_execution_plan(cx));
        assert_eq!(view.execution_plan_finding_count(cx), Some(0));
    });

    view.update(cx, |view, cx| {
        view.set_execution_plan_for_context(
            crate::execution_plan_view::PlanKind::Estimated,
            String::new(),
            vec![sample_execution_plan()],
            cx,
        );
    });
    view.read_with(cx, |view, cx| {
        assert!(!view.is_capturing_execution_plan(cx));
        assert_eq!(
            view.execution_plan_finding_count(cx),
            Some(1),
            "the scan warning counts as one finding"
        );
    });

    view.update(cx, |view, cx| view.toggle_execution_plan_pane(cx));
    view.read_with(cx, |view, _cx| {
        assert!(!view.show_execution_plan, "toggle returns to the grid");
    });
}

/// A gate pushed before the plan pane exists reaches it on creation, and a
/// later change reaches the live pane.
#[gpui::test]
fn plan_capture_gate_reaches_the_plan_pane(cx: &mut gpui::TestAppContext) {
    use crate::capabilities::PlanCaptureGate;
    use crate::execution_plan_view::PlanKind;
    use sqlly_lang::sqlmodel::DatabaseType;
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_plan_capture_gate(PlanCaptureGate::for_engine(Some(DatabaseType::Redis)), cx);
        view.begin_execution_plan_capture(PlanKind::Estimated, cx);
        view.fail_execution_plan(PlanKind::Estimated, "no plan".to_string(), cx);
    });
    view.read_with(cx, |view, cx| {
        let pane = view.execution_plan_view.as_ref().expect("pane created");
        assert!(
            pane.read(cx)
                .recapture_unavailable_reason()
                .is_some_and(|r| r.contains("(Redis)"))
        );
    });
    view.update(cx, |view, cx| {
        view.set_plan_capture_gate(
            PlanCaptureGate::for_engine(Some(DatabaseType::SqlServer)),
            cx,
        );
    });
    view.read_with(cx, |view, cx| {
        let pane = view.execution_plan_view.as_ref().expect("pane");
        assert_eq!(pane.read(cx).recapture_unavailable_reason(), None);
    });
}

#[gpui::test]
fn execution_plan_failure_stops_capturing(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.begin_execution_plan_capture(crate::execution_plan_view::PlanKind::Actual, cx);
        view.fail_execution_plan(
            crate::execution_plan_view::PlanKind::Actual,
            "SHOWPLAN permission denied".to_string(),
            cx,
        );
    });
    view.read_with(cx, |view, cx| {
        assert!(
            !view.is_capturing_execution_plan(cx),
            "a failed capture must not block the next Explain"
        );
        assert!(view.show_execution_plan);
    });
}

#[gpui::test]
#[cfg(not(target_arch = "wasm32"))]
fn start_query_hides_the_plan_pane(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_execution_plan_for_context(
            crate::execution_plan_view::PlanKind::Estimated,
            String::new(),
            vec![sample_execution_plan()],
            cx,
        );
        view.start_query(cx);
    });
    view.read_with(cx, |view, _cx| {
        assert!(
            !view.show_execution_plan,
            "running a query returns the body to the results grid"
        );
    });
}

#[gpui::test]
fn chart_tab_opens_on_the_result_grid_and_clears_on_rerun(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |v, cx| {
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..5), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    // Open the grid's Chart tab the way the context-menu action does.
    view.update(cx, |v, cx| v.open_result_chart_tab(0, cx));
    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    grid.update(cx, |table, cx| {
        assert_eq!(
            table.active_tab(),
            GridTab::Chart,
            "the chart tab comes forward"
        );
        let chart = table.chart_state().expect("chart state exists").clone();
        let chart = chart.read(cx);
        assert!(chart.is_chartable());
        assert_eq!(chart.series_count(), 1, "the int column is the series");
        assert_eq!(chart.category_count(), 5, "the text column labels 5 rows");
    });
    // Switching back to the grid keeps the chart available for re-entry.
    grid.update(cx, |table, cx| {
        table.set_active_tab(GridTab::Grid, cx);
        assert_eq!(table.active_tab(), GridTab::Grid);
        assert!(table.chart_state().is_some());
    });
    // A new run drops the saved chart layouts with the rows they described.
    view.update(cx, |v, cx| v.start_query(cx));
    view.read_with(cx, |v, _cx| {
        assert!(v.chart_configs.is_empty());
    });
}

#[gpui::test]
fn chart_tab_reports_ineligible_sets_honestly(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |v, cx| {
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "name".into(),
                    data_type: "text".into(),
                    nullable: true,
                }],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![
                    vec![CellValue::String("only".into())],
                    vec![CellValue::String("text".into())],
                ],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.update(cx, |v, cx| v.open_result_chart_tab(0, cx));
    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    grid.update(cx, |table, cx| {
        assert_eq!(
            table.active_tab(),
            GridTab::Chart,
            "the tab still opens to say why"
        );
        let chart = table.chart_state().expect("chart state exists").clone();
        let chart = chart.read(cx);
        assert!(!chart.is_chartable());
        assert_eq!(chart.empty_message(), "No numeric columns to chart");
    });
}

#[gpui::test]
#[cfg(not(target_arch = "wasm32"))]
fn historical_results_block_row_editing(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("select 1", cx);
        view.set_historical_results(
            "Viewing results from 14:32:07 — Run to refresh".to_string(),
            vec![crate::query_version_history::SnapshotSet {
                columns: vec![ResultColumn {
                    name: "a".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
                rows: vec![vec![CellValue::I64(1)]],
                rows_affected: None,
            }],
            cx,
        );
        // Must return before emitting EditRowRequested — and must not
        // panic on the historical set.
        view.open_row_editor_with(0, 0, crate::row_editor::RowEditorIntent::Edit, cx);
    });
    view.read_with(cx, |view, _cx| {
        assert!(view.historical_banner().is_some());
    });
}

#[gpui::test]
fn cached_format_hints_match_direct_parse(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let sql = "-- sqlly format: column=Amount; number=2; thousands=false\nSELECT 1";
    view.update(cx, |v, cx| v.set_query_sql(sql, cx));
    view.read_with(cx, |v, _| {
        assert_eq!(v.format_hints, crate::query_format::parse(sql));
    });
}

#[cfg(not(target_arch = "wasm32"))]
#[gpui::test]
fn snapshot_sets_skips_incomplete_sets(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.result_sets = vec![
            ResultSet {
                index: 0,
                columns: vec![],
                rows: vec![vec![CellValue::I64(1)]],
                rows_affected: None,
                row_count_hint: 1,
                ..Default::default()
            },
            // Spill-backed: the hint ran ahead of the resident rows.
            ResultSet {
                index: 1,
                columns: vec![],
                rows: vec![],
                rows_affected: None,
                row_count_hint: 5000,
                ..Default::default()
            },
        ];
        view
    });
    let snap = view.read_with(cx, |view, cx| view.snapshot_sets(cx));
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].rows.len(), 1);
}

/// Regression: a spill-backed set counts as "complete" but holds no resident
/// rows, so history/cache snapshots stored it as an EMPTY set and a tab moved
/// to another window arrived with an empty grid. History skips it; the move
/// snapshot pages a bounded prefix out of the spill file and says so.
#[cfg(not(target_arch = "wasm32"))]
#[gpui::test]
fn spilled_sets_are_skipped_by_history_and_paged_for_a_tab_move(cx: &mut gpui::TestAppContext) {
    let total = 300usize;
    let spill_file = write_test_spill(total);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::SpillManifest {
                manifest: sqlly_services::query::SpillManifestData {
                    result_sets: vec![sqlly_services::query::SpillResultSetData {
                        result_set_index: 0,
                        columns: vec![
                            ResultColumn {
                                name: "id".into(),
                                data_type: "int".into(),
                                nullable: false,
                            },
                            ResultColumn {
                                name: "name".into(),
                                data_type: "text".into(),
                                nullable: true,
                            },
                        ],
                        row_count: total,
                        spill_file: spill_file.clone(),
                    }],
                    messages: Vec::new(),
                    error: None,
                    elapsed_ms: Some(1),
                },
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert!(v.spill_backings[0].is_some(), "the set is spill-backed");
        assert!(
            v.snapshot_sets(cx).is_empty(),
            "history never stores a spilled set as an empty one"
        );
        let (sets, truncated) = v.snapshot_sets_for_move(100, cx);
        assert_eq!(sets.len(), 1);
        assert_eq!(
            sets[0].rows.len(),
            100,
            "bounded prefix from the spill file"
        );
        assert_eq!(sets[0].rows[0][0], CellValue::I64(0));
        assert!(truncated);
        let (sets, truncated) = v.snapshot_sets_for_move(1_000, cx);
        assert_eq!(sets[0].rows.len(), total);
        assert!(!truncated, "the whole set fit under the cap");
    });
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// A bare view for pure-logic tests that need no entity/window context.
/// Mirrors `ResultsView::new`; customize fields via struct-update syntax.
fn bare_results_view() -> ResultsView {
    ResultsView {
        result_sets: vec![],
        active_tab: 0,
        messages: vec![],
        diagnostics: vec![],
        execution_error_offset: None,
        show_messages: false,
        elapsed_ms: None,
        query_start: None,
        first_response_ms: None,
        response_finished_ms: None,
        completed_at: None,
        completed_wall: None,
        run_started_wall: None,
        ui_paint_ms: None,
        wall_clock_ms: None,
        estimated_latency_ms: None,
        subtract_latency: false,
        timing_format: TimingFormat::default(),
        is_running: false,
        error: None,
        historical_banner: None,
        formatting: None,
        masking_rules: None,
        masking_rules_fallback: std::cell::OnceCell::new(),
        selection_stats_cache: std::cell::RefCell::new(HashMap::new()),
        masking_overrides: MaskingOverrides::default(),
        revealed: Vec::new(),
        revealed_cells: Vec::new(),
        column_profiles: Vec::new(),
        column_profile_requested: Vec::new(),
        suggested_mask_accepted: Vec::new(),
        mask_suggestion_dismissed: Vec::new(),
        chart_suggestions: Vec::new(),
        chart_suggestion_requested: Vec::new(),
        chart_suggestion_reverted: Vec::new(),
        copy_format_override: None,
        pending: crate::pending_changes::PendingHistory::default(),
        pending_slot: None,
        staging_slot: None,
        show_preview_sql: false,
        preview_sql_text: String::new(),
        preview_sql_generation: 0,
        preview_sql_panel: None,
        show_detailed_timing: false,
        perf_prefs: crate::preferences::PerformancePreferences::default(),
        gesture_prefs: crate::gesture_prefs::GesturePreferences::defaults(),
        format_hints: Vec::new(),
        grid_directives: Vec::new(),
        conditional_directives: Vec::new(),
        grid_directives_applied: std::collections::HashSet::new(),
        locked_by_click: false,
        computed_specs: Vec::new(),
        computed_plans: Vec::new(),
        formula_variables: Vec::new(),
        formula_now: 0.0,
        formula_bar: None,
        canvas_layout: None,
        show_export_menu: false,
        last_export_notice: None,
        grid_views: vec![],
        grid_zoom: crate::editor_zoom::DEFAULT_ZOOM,
        zoom_wheel_accumulator: 0.0,
        pinch_zoom_accumulator: 0.0,
        synced_rows: vec![],
        auto_size: Default::default(),
        editor: None,
        layout_mode: LayoutMode::default(),
        query_sql: None,
        auto_refresh_secs: None,
        elapsed_ticker: None,
        total_row_counts: HashMap::new(),
        result_set_names: BTreeMap::new(),
        index_labels: BTreeMap::new(),
        collapsed_cards: vec![],
        source_db_type: None,
        layout_scope: None,
        saved_filter_store_for_test: None,
        layout_store: None,
        layout_store_mode: LayoutStoreMode::Disk,
        background_read: None,
        spill_drills: Vec::new(),
        spill_drill_generation: 0,
        background_read_generation: 0,
        spill_prefixes: std::cell::RefCell::new(Vec::new()),
        applied_layouts: vec![],
        layout_save_generation: vec![],
        fk_label_profile: None,
        fk_label_services: None,
        row_edit_connection_block: None,
        fk_label_cache: crate::fk_labels::LabelCache::default(),
        fk_labels_by_slot: Vec::new(),
        fk_label_fetch_generation: Vec::new(),
        track_difference: false,
        previous_result_sets: vec![],
        diff: None,
        show_diff: false,
        execution_plan_view: None,
        plan_capture_gate: crate::capabilities::PlanCaptureGate::default(),
        show_execution_plan: false,
        show_history: false,
        show_ddl: false,
        #[cfg(not(target_arch = "wasm32"))]
        history_panel: None,
        ddl_panel: None,
        stacked_scroll_handle: ScrollHandle::new(),
        stacked_card_heights: Vec::new(),
        layout_directives: crate::result_layout::LayoutDirectives::default(),
        stacked_resize: None,
        scroll_accumulator: ScrollDeltaAccumulator::default(),
        tab_switch_gate: crate::gesture_interpreter::EdgeSwitchGate::default(),
        result_name_menu: None,
        result_settings: None,
        diff_direction: DiffDirection::default(),
        diff_grid_previous: None,
        diff_grid_current: None,
        diff_grid_diff: None,
        retained_diff_runs: Vec::new(),
        next_diff_run_id: 1,
        diff_baseline_run: None,
        diff_comparison_run: None,
        retained_runs: Vec::new(),
        next_run_id: 1,
        viewing_run: None,
        current_run_snapshot: None,
        run_history_limit: 5,
        run_history_budget_bytes: 64 * 1024 * 1024,
        run_history_enabled: true,
        run_history_capture: crate::preferences::RunHistoryCaptureMode::Either,
        pending_keep_results: false,
        diff_key_columns: Vec::new(),
        detailed_diff: None,
        diff_unavailable_reason: None,
        detailed_diff_grid: None,
        spill_manifest: SpillManifestGuard(None),
        spill_backings: Vec::new(),
        spill_paging: Vec::new(),
        view_hints: None,
        chart_configs: HashMap::new(),
        result_profile_grid: None,
        result_profile_caption: String::new(),
        result_profile: None,
        result_profile_sql: None,
        result_profile_filter: crate::result_profiler::ProfileFilter::default(),
        result_profile_indices: Vec::new(),
        _result_profile_watch: None,
        row_form: None,
        conditional_formatting: None,
        pending_conditional_formatting: None,
        dialog_frames: crate::dialog::DialogFrames::new(),
        cell_detail: None,
        cell_detail_side: crate::cell_detail_model::DockSide::Right,
        cell_detail_size: crate::workspace_session::DEFAULT_CELL_DETAIL_WIDTH,
        last_focus: None,
        pane_maximized: false,
        hydration_requested: None,
        filter_builder: None,
        client_filter: Vec::new(),
        applied_filter_models: std::collections::HashMap::new(),
        source_index_map: Vec::new(),
        sort_specs: Vec::new(),
        transposed: vec![],
        transpose_active: vec![],
        transpose_truncated: vec![],
        transpose_row_count: vec![],
        filter_limitations: Vec::new(),
        default_filter_consulted: Default::default(),
        result_epoch: 0,
        spill_install_pending: false,
        snapshot_prompt: None,
        diff_class_filter: None,
        diff_snapshot_baseline: None,
        diff_snapshot_index: None,
        show_result_profile: false,
        show_statistics: false,
        statistics_opened: false,
        streaming_notify_pending: false,
        streaming_notify_task: None,
        run_generation: 0,
        service_owned_spill_files: Default::default(),
        messages_input: None,
        messages_rendered: SharedString::default(),
        grid_find: Default::default(),
        goto_column: None,
    }
}

/// Writes a complete typed spill file of two-column rows plus its `.idx`
/// sidecar, returning the data-file path.
/// Runs a native spill read, which never waits, to its result.
fn ready<T>(read: impl std::future::Future<Output = T>) -> T {
    let mut read = std::pin::pin!(read);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match read.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("a native spill read waited"),
    }
}

fn write_test_spill(total: usize) -> String {
    // Unique per call: parallel tests can share a clock timestamp, and a
    // colliding path means one test's cleanup deletes another's file.
    static SPILL_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SPILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "results_spill_test_{}_{}_{}.bin",
        std::process::id(),
        seq,
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut writer = sqlly_core::SpillWriter::create(&path).expect("spill file");
    for i in 0..total {
        writer
            .append(&[
                CellValue::I64(i as i64),
                CellValue::String(format!("row{i}")),
            ])
            .expect("append");
    }
    writer.finish().expect("finish");
    path.to_string_lossy().to_string()
}

/// End-to-end spill-backed windowing: a manifest for a 200k-row set must
/// switch the grid to windowed mode (virtual total, small resident
/// window, empty `rs.rows`), and scrolling deep into the set must page
/// the window there via `maybe_page_spill` (fired by the grid observers
/// on every notify in the app).
#[gpui::test]
fn spill_manifest_windows_grid_and_scroll_pages_rows(cx: &mut gpui::TestAppContext) {
    let total = 200_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        // The under-threshold head streamed normally...
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();

    let manifest_event = QueryEvent::SpillManifest {
        manifest: sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                row_count: total,
                spill_file: spill_file.clone(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: Some(1),
        },
    };
    view.update(cx, |v, cx| {
        v.handle_event(manifest_event, cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    // Windowed mode: full virtual total, bounded resident rows, freed rows.
    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    view.read_with(cx, |v, cx| {
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.display_row_count(), total, "grid presents the full set");
        assert!(
            s.data.rows.len() < 10_000,
            "only a window is resident, got {}",
            s.data.rows.len()
        );
        assert_eq!(s.data.rows[0][0], sqlly_datatable::CellValue::Integer(0));
        assert!(
            v.result_sets[0].rows.is_empty(),
            "in-memory rows must be freed for spill-backed sets"
        );
    });

    // Jump the scroll deep into un-paged territory. The GridState notify
    // alone must reach the paging hook: real scrolls notify the STATE
    // entity, not the wrapper, so this intentionally does NOT call
    // `maybe_page_spill` — it exercises the observer wiring end-to-end
    // (regression: paging observers were once attached to the wrapper
    // entity only and never fired on real scrolls).
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());
    grid_state.update(cx, |s, cx| {
        let row_h = s.row_height;
        s.scroll_handle.set_offset(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(150_000.0 * row_h),
        });
        cx.notify();
    });
    cx.run_until_parked();
    // Scroll paging is async: the debounce timer must fire and the
    // background read complete before the window lands.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    view.read_with(cx, |_v, cx| {
        let s = grid.read(cx).state.read(cx);
        let w = s.window.expect("still windowed");
        assert!(
            w.offset > 100_000,
            "window must have paged toward the scroll target, offset={}",
            w.offset
        );
        let resident = s
            .resident_row_for_display(150_000)
            .expect("row 150000 resident after paging");
        assert_eq!(
            s.data.rows[resident][1],
            sqlly_datatable::CellValue::Text("row150000".into())
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// A manifest delivered for a superseded run must delete its spill files:
/// the stale pump keeps draining after a re-run, its backings were dropped
/// by `start_query`'s clear, and nothing else owns the files.
#[gpui::test]
fn stale_manifest_files_are_deleted_on_superseded_run(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(50);
    let idx_file = format!("{spill_file}.idx");
    assert!(std::path::Path::new(&spill_file).exists());

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let manifest = spill_manifest_event(&spill_file, 50);
    view.update(cx, |v, cx| {
        let stale_run = v.run_generation();
        v.start_query(cx); // supersedes stale_run
        v.handle_event_for_run(stale_run, manifest, cx);
    });
    cx.run_until_parked();

    assert!(
        !std::path::Path::new(&spill_file).exists(),
        "stale manifest's spill file must be deleted"
    );
    assert!(
        !std::path::Path::new(&idx_file).exists(),
        "stale manifest's index sidecar must be deleted"
    );
}

/// Builds the `SpillManifest` query event for a single two-column set.
fn spill_manifest_event(spill_file: &str, total: usize) -> QueryEvent {
    QueryEvent::SpillManifest {
        manifest: sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                row_count: total,
                spill_file: spill_file.to_string(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: Some(1),
        },
    }
}

/// Rapid scrolling must coalesce: a burst of desired-window requests
/// while the debounce timer is armed spawns exactly ONE background read
/// (the slot generation advances once), and the applied window is the
/// LAST requested one.
#[gpui::test]
fn spill_page_burst_coalesces_to_one_read_of_last_window(cx: &mut gpui::TestAppContext) {
    let total = 200_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());
    let gen_before = view.read_with(cx, |v, _cx| v.spill_paging[0].generation);

    // Three scroll targets in quick succession — no clock advance between.
    for target in [50_000usize, 100_000, 150_000] {
        grid_state.update(cx, |s, cx| {
            let row_h = s.row_height;
            s.scroll_handle.set_offset(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(target as f32 * row_h),
            });
            cx.notify();
        });
        view.update(cx, |v, cx| v.maybe_page_spill(0, cx));
    }
    view.read_with(cx, |v, _cx| {
        let st = &v.spill_paging[0];
        assert!(st.pending.is_some(), "latest request recorded");
        assert!(st.debounce.is_some(), "one timer armed for the whole burst");
        assert_eq!(
            st.generation, gen_before,
            "no read spawned before the debounce fires"
        );
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert_eq!(
            v.spill_paging[0].generation,
            gen_before + 1,
            "the burst spawned exactly one read"
        );
        assert!(
            v.spill_paging[0].read.is_none(),
            "read completed and applied"
        );
        let s = grid.read(cx).state.read(cx);
        let w = s.window.expect("still windowed");
        assert!(
            w.offset > 100_000,
            "window targets the LAST request, offset={}",
            w.offset
        );
        let resident = s
            .resident_row_for_display(150_000)
            .expect("row 150000 resident after the coalesced page");
        assert_eq!(
            s.data.rows[resident][1],
            sqlly_datatable::CellValue::Text("row150000".into())
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// A completed read whose generation was superseded must be discarded:
/// the grid window stays untouched even though the rows were valid.
#[gpui::test]
fn stale_spill_page_apply_is_discarded(cx: &mut gpui::TestAppContext) {
    let total = 10_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let offset_before = view.read_with(cx, |_v, cx| {
        grid.read(cx)
            .state
            .read(cx)
            .window
            .expect("windowed")
            .offset
    });

    view.update(cx, |v, cx| {
        let stale = v.spill_paging[0].generation;
        // A newer request supersedes the (simulated) in-flight read.
        v.spill_paging[0].generation += 1;
        let rows = vec![
            vec![
                sqlly_datatable::CellValue::Integer(-1),
                sqlly_datatable::CellValue::Text("stale".into()),
            ];
            5
        ];
        v.apply_spill_page(0, stale, 5_000, total, Some(rows), cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |_v, cx| {
        let s = grid.read(cx).state.read(cx);
        let w = s.window.expect("still windowed");
        assert_eq!(
            w.offset, offset_before,
            "a stale apply must not move the window"
        );
        assert_eq!(
            s.data.rows[0][0],
            sqlly_datatable::CellValue::Integer(0),
            "resident rows untouched by the stale apply"
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Matching-generation `None` (open/IO failure) is visible in Messages,
/// once per slot — a second apply of the same failure must not duplicate.
#[gpui::test]
fn matching_generation_spill_page_none_is_reported_once(cx: &mut gpui::TestAppContext) {
    let total = 10_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        let generation = v.spill_paging[0].generation;
        v.apply_spill_page(0, generation, 0, total, None, cx);
        v.apply_spill_page(0, generation, 0, total, None, cx);
    });

    view.read_with(cx, |v, _cx| {
        let hits = v
            .messages
            .iter()
            .filter(|m| m.contains("could not read this page of the result from disk"))
            .count();
        assert_eq!(
            hits, 1,
            "one Messages line, not one per apply, got {:?}",
            v.messages
        );
        assert!(v.show_messages);
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// A superseded page-read `None` must stay quiet — generation mismatch
/// is not a user-visible failure.
#[gpui::test]
fn superseded_spill_page_none_stays_quiet(cx: &mut gpui::TestAppContext) {
    let total = 10_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        let stale = v.spill_paging[0].generation;
        v.spill_paging[0].generation += 1;
        v.apply_spill_page(0, stale, 0, total, None, cx);
    });

    view.read_with(cx, |v, _cx| {
        assert!(
            v.messages
                .iter()
                .all(|m| !m.contains("could not read this page of the result from disk")),
            "superseded None must not message, got {:?}",
            v.messages
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// `clear()` (the new-query path) between a scheduled page request and
/// its debounce/read must cancel everything: no panic, nothing applied,
/// and the manifest's spill files are deleted despite the in-flight
/// request.
#[gpui::test]
fn clear_mid_flight_cancels_spill_paging_and_applies_nothing(cx: &mut gpui::TestAppContext) {
    let total = 200_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());
    grid_state.update(cx, |s, cx| {
        let row_h = s.row_height;
        s.scroll_handle.set_offset(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(150_000.0 * row_h),
        });
        cx.notify();
    });
    view.update(cx, |v, cx| v.maybe_page_spill(0, cx));
    view.read_with(cx, |v, _cx| {
        assert!(
            v.spill_paging[0].debounce.is_some(),
            "page request scheduled before the clear"
        );
    });

    view.update(cx, |v, cx| v.clear(cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert!(v.spill_backings.is_empty(), "backings dropped by clear");
        assert!(v.grid_views.is_empty(), "grids dropped by clear");
        assert!(
            v.spill_paging
                .iter()
                .all(|st| st.debounce.is_none() && st.read.is_none() && st.pending.is_none()),
            "all paging tasks cancelled by clear"
        );
    });
    assert!(
        !std::path::Path::new(&spill_file).exists(),
        "clear() deleted the spill file"
    );

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Mid-query `SpillWindow`: the first receipt installs a PROVISIONAL
/// backing over the flushed prefix — grid windowed at `rows_available`,
/// streamed preview rows freed, NO manifest guard (files are still
/// service-owned) — and later receipts grow the virtual total in place
/// without recreating the grid or moving the scroll.
#[gpui::test]
fn spill_window_installs_provisional_backing_and_grows_total(cx: &mut gpui::TestAppContext) {
    // The on-disk file is AHEAD of the advertised prefix, as mid-write.
    let spill_file = write_test_spill(5_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 1_000,
            },
            cx,
        );
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let first_entity = grid.entity_id();
    view.read_with(cx, |v, cx| {
        let backing = v.spill_backings[0].as_ref().expect("provisional backing");
        assert_eq!(backing.total, 1_000, "total is the flushed prefix");
        assert!(
            v.spill_manifest.0.is_none(),
            "no guard mid-query — the file is still service-owned"
        );
        assert!(
            v.result_sets[0].rows.is_empty(),
            "streamed preview rows freed at provisional install"
        );
        assert_eq!(v.row_count_for_slot(0), 1_000);
        let fmt = ResultFormattingPreferences::default();
        assert_eq!(
            v.result_count_label(0, &fmt),
            format!("Result 1: {} row(s)", fmt.format_integer(1_000)),
            "count label must survive freeing rs.rows at provisional install"
        );
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.display_row_count(), 1_000, "grid windowed at the prefix");
        assert!(s.window.is_some());
        assert_eq!(s.data.rows[0][0], sqlly_datatable::CellValue::Integer(0));
    });

    // Later ticks only grow the total.
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 2_500,
            },
            cx,
        );
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert_eq!(v.spill_backings[0].as_ref().unwrap().total, 2_500);
        assert_eq!(v.row_count_for_slot(0), 2_500);
        let grid = v.grid_views[0].as_ref().expect("grid still exists");
        assert_eq!(
            grid.entity_id(),
            first_entity,
            "growth must not recreate the grid entity"
        );
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.display_row_count(), 2_500, "virtual total grew in place");
        assert_eq!(
            s.window.expect("still windowed").offset,
            0,
            "resident window untouched by growth"
        );
        assert_eq!(
            f32::from(s.scroll_handle.offset().y),
            0.0,
            "scroll not reset by growth"
        );
    });

    assert!(
        std::path::Path::new(&spill_file).exists(),
        "provisional backing never deletes the service-owned file"
    );
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Scroll paging against a provisional backing must clamp to
/// `rows_available` even though the on-disk file (and the reader's own
/// row count) already contains more rows — reading past the flushed
/// prefix is unsafe mid-write.
#[gpui::test]
fn spill_window_scroll_paging_clamps_to_rows_available(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(20_000);
    let advertised = 10_000usize;

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: advertised,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());
    grid_state.update(cx, |s, cx| {
        let row_h = s.row_height;
        s.scroll_handle.set_offset(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(9_500.0 * row_h),
        });
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| v.maybe_page_spill(0, cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    view.read_with(cx, |_v, cx| {
        let s = grid.read(cx).state.read(cx);
        assert_eq!(
            s.display_row_count(),
            advertised,
            "virtual total stays at the advertised prefix, not the file's"
        );
        let w = s.window.expect("windowed");
        assert!(
            w.offset + s.data.rows.len() <= advertised,
            "resident window must end at/before rows_available, got {}..{}",
            w.offset,
            w.offset + s.data.rows.len()
        );
        let resident = s
            .resident_row_for_display(9_999)
            .expect("last advertised row resident after paging");
        assert_eq!(
            s.data.rows[resident][1],
            sqlly_datatable::CellValue::Text("row9999".into())
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// The final manifest for a set with a provisional backing finalizes in
/// place: authoritative total, guard installed (ownership transferred),
/// same grid entity, scroll untouched — no visible reset.
#[gpui::test]
fn spill_manifest_finalizes_provisional_backing_without_reset(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(5_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 2_000,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let first_entity = grid.entity_id();
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());
    let scroll_y = grid_state.update(cx, |s, cx| {
        let row_h = s.row_height;
        s.scroll_handle.set_offset(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(500.0 * row_h),
        });
        cx.notify();
        f32::from(s.scroll_handle.offset().y)
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, 5_000), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert!(
            v.spill_manifest.0.is_some(),
            "manifest guard owns the files now"
        );
        assert_eq!(
            v.spill_backings[0].as_ref().expect("backing").total,
            5_000,
            "authoritative manifest count overwrites the provisional total"
        );
        assert_eq!(v.row_count_for_slot(0), 5_000);
        let grid = v.grid_views[0].as_ref().expect("grid exists");
        assert_eq!(
            grid.entity_id(),
            first_entity,
            "finalizing must not recreate the grid entity"
        );
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.display_row_count(), 5_000);
        assert_eq!(
            f32::from(s.scroll_handle.offset().y),
            scroll_y,
            "scroll position survives finalization"
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// `clear()` while a provisional backing is installed (query still
/// running) must drop the backing and reader WITHOUT deleting the
/// service-owned files — there is no guard yet, and deletion ownership
/// only transfers with the manifest.
#[gpui::test]
fn clear_with_provisional_backing_leaves_service_owned_files(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(5_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 2_000,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| v.clear(cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert!(v.spill_backings.is_empty(), "provisional backing dropped");
        assert!(
            v.spill_paging
                .iter()
                .all(|st| st.debounce.is_none() && st.read.is_none() && st.pending.is_none()),
            "paging tasks cancelled"
        );
    });
    assert!(
        std::path::Path::new(&spill_file).exists(),
        "clear must NOT delete a service-owned (pre-manifest) spill file"
    );

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Whole-set consumers (toolbar export/copy and the grid context menu's
/// export, via `spill_rows_for_slot`) must read the backing's rows for a
/// spill-backed slot — `rs.rows` is empty — clamped to the advertised
/// flushed prefix even when the on-disk file is ahead of it.
#[gpui::test]
fn whole_set_consumers_page_provisional_backing_clamped(cx: &mut gpui::TestAppContext) {
    // File holds 3_000 rows; only 1_200 are advertised as flushed.
    let spill_file = write_test_spill(3_000);
    let advertised = 1_200usize;

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: advertised,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert!(v.result_sets[0].rows.is_empty(), "rows freed at install");

        // Toolbar export/copy path.
        let visible = v.visible_rows_for_slot(0, cx);
        assert_eq!(visible.len(), advertised, "clamped to the flushed prefix");
        assert_eq!(visible[0][0], CellValue::I64(0));
        assert_eq!(
            visible[advertised - 1][1],
            CellValue::String(format!("row{}", advertised - 1))
        );

        // Context-menu export path (cannot touch the grid entity).
        let menu_rows = v.spill_rows_for_slot(0).expect("slot is spill-backed");
        assert_eq!(menu_rows.len(), advertised);
        assert_eq!(
            menu_rows[advertised - 1][0],
            CellValue::I64((advertised - 1) as i64)
        );
        assert!(
            v.spill_rows_for_slot(1).is_none(),
            "non-backed slots return None so the menu uses rs.rows"
        );

        // export_string ties the whole toolbar chain together.
        let csv = v
            .export_string(ExportFormat::Csv, cx)
            .expect("csv export of a backed set");
        assert_eq!(
            csv.lines().count(),
            advertised + 1,
            "header + one line per flushed row"
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Spill-backed copy/export must page a prefix via `first_rows`, never
/// `all_rows()`, and report `truncated` when the file is larger than the
/// cap. In-memory grids stay uncapped.
#[gpui::test]
fn spill_export_cap_sets_truncated_and_leaves_in_memory_uncapped(cx: &mut gpui::TestAppContext) {
    assert_eq!(
        result_export::EXPORT_MAX_ROWS,
        crate::result_snapshots::SNAPSHOT_MAX_ROWS,
        "export and snapshots share one named bound"
    );
    let spill_file = write_test_spill(25);
    let cap = 10usize;

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 25,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        let (rows, truncated) = v.visible_rows_for_slot_capped(0, cap, cx);
        assert!(truncated, "spill larger than cap must set truncated");
        assert_eq!(rows.len(), cap);
        assert_eq!(rows[0][0], CellValue::I64(0));
        assert_eq!(rows[cap - 1][0], CellValue::I64((cap - 1) as i64));

        let (csv, csv_truncated) = v
            .export_string_capped(ExportFormat::Csv, cap, cx)
            .expect("csv export of a capped spill");
        assert!(csv_truncated);
        assert_eq!(csv.lines().count(), cap + 1, "header + capped rows");

        let (_full, full_truncated) = v
            .export_string_capped(ExportFormat::Csv, result_export::EXPORT_MAX_ROWS, cx)
            .expect("csv under the production cap");
        assert!(!full_truncated, "a 25-row spill is under EXPORT_MAX_ROWS");
    });

    view.update(cx, |v, cx| v.note_truncated_export_for_slot(0, cx));
    view.read_with(cx, |v, _cx| {
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("stay on disk") && m.contains("25")),
            "truncation must be visible in Messages, got {:?}",
            v.messages
        );
        assert!(v.show_messages);
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();

    let (view, cx) = make_results_view_with_data(cx);
    view.read_with(cx, |v, cx| {
        let (rows, truncated) = v.visible_rows_for_slot_capped(0, 1, cx);
        assert!(!truncated, "in-memory grids must not be capped");
        assert_eq!(rows.len(), 3, "all in-memory rows remain visible");
        let csv = v.export_string(ExportFormat::Csv, cx).expect("csv");
        assert_eq!(csv.lines().count(), 4);
    });
}

/// Events tagged with a superseded run stamp are dropped wholesale: a
/// still-draining old pump's `Rows`/`SpillWindow`/`SpillManifest` must
/// not install the OLD query's spill file (or rows) into the NEW run's
/// same-index result set.
#[gpui::test]
fn stale_run_events_are_dropped_by_run_stamp(cx: &mut gpui::TestAppContext) {
    let old_spill = write_test_spill(1_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    cx.run_until_parked();

    let (run_a, run_b) = view.update(cx, |v, cx| {
        v.start_query(cx);
        let run_a = v.run_generation();
        v.handle_event_for_run(run_a, QueryEvent::Started, cx);
        v.handle_event_for_run(run_a, two_column_result_set(), cx);
        // Query B supersedes A while A's pump is still draining.
        v.start_query(cx);
        let run_b = v.run_generation();
        v.handle_event_for_run(run_b, QueryEvent::Started, cx);
        v.handle_event_for_run(run_b, two_column_result_set(), cx);
        v.handle_event_for_run(run_b, rows_batch(0..5), cx);
        (run_a, run_b)
    });
    assert_ne!(run_a, run_b);

    view.update(cx, |v, cx| {
        // A's queued SpillWindow tick lands AFTER B's set 0 exists.
        v.handle_event_for_run(
            run_a,
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: old_spill.clone(),
                rows_available: 1_000,
            },
            cx,
        );
        // A's stale rows and trailing manifest are dropped too.
        v.handle_event_for_run(run_a, rows_batch(100..200), cx);
        v.handle_event_for_run(run_a, spill_manifest_event(&old_spill, 1_000), cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert!(
            v.spill_backings.iter().all(Option::is_none),
            "stale SpillWindow must not hijack the new run's slot"
        );
        assert!(
            v.spill_manifest.0.is_none(),
            "stale manifest must not take file ownership in the new run"
        );
        assert_eq!(
            v.result_sets[0].rows.len(),
            5,
            "B's streamed rows stay on the result set so reveal can restore raw cells"
        );
        assert_eq!(v.row_count_for_slot(0), 5);
        assert_eq!(v.synced_rows[0], 5);
    });

    assert!(
        std::path::Path::new(&old_spill).exists(),
        "the old run's service-owned file is untouched"
    );
    std::fs::remove_file(&old_spill).ok();
    std::fs::remove_file(format!("{old_spill}.idx")).ok();
}

/// A growth tick whose pager reopen fails must NOT grow the backing
/// total past the pager's snapshot — whole-set consumers (export, stats)
/// would silently truncate below the advertised count. The live count
/// still advances via the row-count hint.
#[gpui::test]
fn spill_window_growth_holds_total_when_reopen_fails(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(2_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 1_000,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    // The next tick's reopen fails (file vanished / unreadable).
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 1_500,
            },
            cx,
        );
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        let backing = v.spill_backings[0].as_ref().expect("backing kept");
        assert_eq!(
            backing.total, 1_000,
            "total must not grow past the pager's snapshot on reopen failure"
        );
        assert_eq!(
            backing.all_rows().len(),
            1_000,
            "whole-set reads still cover the full advertised total"
        );
        let grid = v.grid_views[0].as_ref().expect("grid exists");
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.display_row_count(), 1_000, "virtual total held too");
        assert_eq!(
            v.row_count_for_slot(0),
            1_500,
            "live count still advances via the hint"
        );
    });
}

/// The count-only `RowsProgress` fallback (flush failure) must keep the
/// visible count advancing for a provisionally backed slot instead of
/// freezing it at the flushed-prefix total.
#[gpui::test]
fn rows_progress_advances_count_past_provisional_backing_total(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(2_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 1_000,
            },
            cx,
        );
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 3_000,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert_eq!(v.spill_backings[0].as_ref().unwrap().total, 1_000);
        assert_eq!(
            v.row_count_for_slot(0),
            3_000,
            "count-only progress must advance the live count"
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// If the FINAL manifest install fails to open a set's file after the
/// provisional backing already freed the streamed rows, the deletion
/// guard must not take that file (the on-disk copy is the only one
/// left), and the failure must be surfaced.
#[gpui::test]
fn manifest_finalize_open_failure_keeps_file_out_of_guard(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(2_000);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: 1_000,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert!(
            v.result_sets[0].rows.is_empty(),
            "provisional install freed the streamed rows"
        );
    });

    // Corrupt the file so the finalize-time open fails while the file
    // itself still exists (e.g. torn header / AV interference).
    std::fs::write(&spill_file, b"garbage").expect("corrupt spill file");
    std::fs::remove_file(format!("{spill_file}.idx")).ok();

    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, 2_000), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert!(v.spill_backings[0].is_none(), "no backing installed");
        let guard = v.spill_manifest.0.as_ref().expect("manifest recorded");
        assert!(
            guard.result_sets.is_empty(),
            "guard must not own a file whose rows exist nowhere else"
        );
        assert!(
            v.error.is_some(),
            "finalize failure with freed rows must be surfaced"
        );
    });

    // Guard cleanup (next query / teardown) must leave the file alone.
    view.update(cx, |v, cx| v.clear(cx));
    cx.run_until_parked();
    assert!(
        std::path::Path::new(&spill_file).exists(),
        "orphaned spill file is preserved on disk, not deleted"
    );
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Selection stats on a spill-backed slot: cell-range selections page
/// their rows from the backing (clamped to the flushed prefix), while
/// whole-column selections yield no stats — deliberately disabled rather
/// than paging the entire file on the UI thread.
#[gpui::test]
fn selection_stats_on_spill_backed_slot(cx: &mut gpui::TestAppContext) {
    let spill_file = write_test_spill(3_000);
    let advertised = 1_000usize;

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: advertised,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let grid_state = grid.read_with(cx, |table, _| table.state.clone());

    // Cell-range selection over the integer column pages from the file.
    grid_state.update(cx, |s, cx| {
        s.selection = sqlly_datatable::Selection::CellRange(10, 0, 12, 0);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        let stats = v.selection_stats_text(cx).expect("range stats paged");
        // Rows 10..=12, col 0 hold 10, 11, 12 — the sum must show up.
        assert!(stats.contains("33"), "unexpected stats text: {stats}");
    });

    // A range reaching past the flushed prefix only aggregates the
    // safe rows (reads at/after `rows_available` are clamped off).
    grid_state.update(cx, |s, cx| {
        s.selection = sqlly_datatable::Selection::CellRange(advertised - 1, 0, advertised + 50, 0);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        let stats = v.selection_stats_text(cx).expect("clamped range stats");
        let expected = ResultFormattingPreferences::default().format_integer(999);
        assert!(
            stats.contains(&expected),
            "only the one in-prefix row (value 999) aggregates: {stats}"
        );
    });

    // Whole-column selection: rs.rows is empty, so no stats (by design).
    grid_state.update(cx, |s, cx| {
        s.selection = sqlly_datatable::Selection::Column(0);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert!(
            v.selection_stats_text(cx).is_none(),
            "column stats are disabled for spill-backed slots"
        );
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// Difference tracking must not diff incomplete rows: a previous set
/// cloned from a spill-backed slot (empty rows, large hint) or a
/// suppressed current set would produce a bogus "everything changed"
/// diff. Complete sets still diff.
#[gpui::test]
fn diff_skipped_when_either_side_has_incomplete_rows(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.track_difference = true;
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..10), cx);
        v
    });
    cx.run_until_parked();

    // Previous set mimics a clone of a spill-backed slot: rows freed,
    // logical count carried in the hint.
    view.update(cx, |v, cx| {
        let mut prev = v.result_sets.clone();
        prev[0].rows = Vec::new();
        prev[0].row_count_hint = 50_000;
        v.previous_result_sets = prev;
        assert!(!v.previous_result_sets[0].rows_complete());
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert!(v.diff.is_none(), "incomplete previous rows must not diff");
        assert!(!v.show_diff);
    });

    // Control: with a complete previous set the diff computes.
    view.update(cx, |v, cx| {
        v.is_running = true;
        let mut prev: Vec<ResultSet> = v
            .result_sets
            .iter()
            .enumerate()
            .map(|(slot, rs)| {
                let mut cloned = rs.clone();
                cloned.rows = v.resident_source_rows_for_slot(slot, cx);
                cloned
            })
            .collect();
        prev[0].rows.pop();
        prev[0].row_count_hint = prev[0].rows.len();
        v.previous_result_sets = prev;
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert!(v.diff.is_some(), "complete sets must still diff");
    });
}

/// Difference tracking is memory-bounded: a result set past
/// `DIFF_MAX_ROWS` is never diffed (the diff clones and multiset-groups
/// both sides), and `start_query` drops the oversized rows instead of
/// retaining a full previous copy. Sets within the cap still diff.
#[gpui::test]
fn diff_skipped_when_result_set_exceeds_row_cap(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.track_difference = true;
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..10), cx);
        v
    });
    cx.run_until_parked();

    // Previous set is small and complete; the current set is inflated
    // past the cap (fully resident, so `rows_complete()` still holds —
    // only the cap can refuse it).
    let oversized = crate::result_set_difference::DIFF_MAX_ROWS + 1;
    view.update(cx, |v, cx| {
        v.previous_result_sets = v.result_sets.clone();
        let filler = vec![CellValue::I64(0), CellValue::String("row0".into())];
        v.result_sets[0].rows = vec![filler; oversized];
        v.result_sets[0].row_count_hint = oversized;
        assert!(v.result_sets[0].rows_complete());
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert!(v.diff.is_none(), "oversized current set must not diff");
        assert!(!v.show_diff);
    });

    // The next run must not retain the oversized rows as "previous":
    // the slot survives for index alignment but its payload is freed,
    // and the preserved logical count marks it incomplete/over-cap.
    view.update(cx, |v, cx| {
        v.start_query(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        let prev = &v.previous_result_sets[0];
        assert!(
            prev.rows.is_empty(),
            "oversized rows must not be retained for diffing"
        );
        assert_eq!(prev.row_count_hint, oversized);
        assert!(!prev.rows_complete());
    });

    // Control: rebuild both sides within the cap and the diff computes.
    view.update(cx, |v, cx| {
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..10), cx);
        v.previous_result_sets = v
            .result_sets
            .iter()
            .enumerate()
            .map(|(slot, rs)| {
                let mut cloned = rs.clone();
                cloned.rows = v.resident_source_rows_for_slot(slot, cx);
                cloned
            })
            .collect();
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert!(v.diff.is_some(), "sets within the cap must still diff");
    });
}

#[gpui::test]
fn retained_diff_runs_allow_arbitrary_selection_and_composite_keys(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.track_difference = true;
        for (name, active) in [("Ada", false), ("Grace", true), ("Lin", true)] {
            view.result_sets = vec![ResultSet {
                index: 0,
                columns: vec![
                    ResultColumn {
                        name: "tenant".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                    ResultColumn {
                        name: "active".into(),
                        data_type: "bit".into(),
                        nullable: false,
                    },
                ],
                rows: vec![vec![
                    CellValue::I64(7),
                    CellValue::I64(1),
                    CellValue::String(name.into()),
                    CellValue::Bool(active),
                ]],
                rows_affected: None,
                row_count_hint: 1,
                ..Default::default()
            }];
            view.retain_completed_diff_run(cx);
        }
        view
    });
    cx.run_until_parked();

    view.update(cx, |view, cx| {
        assert_eq!(view.retained_diff_runs.len(), 3);
        assert_eq!(view.diff_baseline_run, Some(2));
        assert_eq!(view.diff_comparison_run, Some(3));
        view.cycle_diff_run(true, cx);
        assert_eq!(view.diff_baseline_run, Some(3));
        view.cycle_diff_run(true, cx);
        assert_eq!(view.diff_baseline_run, Some(1));
        view.toggle_diff_key_column(0, cx);
        view.toggle_diff_key_column(1, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _cx| {
        let diff = view.detailed_diff.as_ref().expect("comparison computed");
        assert_eq!(diff.key_columns, vec![0, 1]);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].changes.len(), 2);
        assert!(view.detailed_diff_grid.is_some());
    });
}

#[gpui::test]
fn retained_diff_history_is_bounded_and_keeps_unavailable_reason(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.track_difference = true;
        for id in 0..=result_set_difference::DIFF_MAX_RUNS {
            view.result_sets = vec![ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
                rows: vec![vec![CellValue::I64(id as i64)]],
                rows_affected: None,
                row_count_hint: 1,
                ..Default::default()
            }];
            view.retain_completed_diff_run(cx);
        }
        view.result_sets[0].row_count_hint = 2;
        view.retain_completed_diff_run(cx);
        view
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _cx| {
        assert_eq!(
            view.retained_diff_runs.len(),
            result_set_difference::DIFF_MAX_RUNS
        );
        assert_eq!(view.retained_diff_runs.first().unwrap().id, 3);
        assert!(
            view.retained_diff_runs.last().unwrap().sets[0]
                .rows
                .is_none()
        );
        assert!(
            view.diff_unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("did not retain every row"))
        );
    });
}

#[gpui::test]
fn completed_spill_manifest_marks_retained_run_unavailable(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let spill_file = write_test_spill(10);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.track_difference = true;
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..10), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.handle_event(spill_manifest_event(&spill_file, 10), cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _cx| {
        let retained = &view.retained_diff_runs.last().unwrap().sets[0];
        assert!(retained.rows.is_none());
        assert!(
            retained
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("spill-backed"))
        );
    });
    drop(view);
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// The pivot export table is the PIVOTED shape (what the pivot grid
/// shows), not the source rows, and refuses incomplete sets.
#[gpui::test]
fn pivot_export_table_returns_pivoted_shape(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.view_hints = Some(crate::result_pivot::ResultViewHints {
            default_view: crate::result_pivot::ResultViewKind::Grid,
            pivot: Some(crate::result_pivot::ResultPivotSpec::new(
                "region",
                "name",
                "amount",
                crate::result_pivot::ResultPivotAggregate::Sum,
            )),
            chart: None,
            diagnostics: vec![],
        });
        v.result_sets = vec![ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "region".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "name".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "amount".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
            ],
            rows: vec![
                vec![
                    CellValue::String("east".into()),
                    CellValue::String("a".into()),
                    CellValue::I64(5),
                ],
                vec![
                    CellValue::String("west".into()),
                    CellValue::String("b".into()),
                    CellValue::I64(7),
                ],
            ],
            rows_affected: None,
            row_count_hint: 2,
            ..Default::default()
        }];
        v
    });
    view.update(cx, |v, cx| {
        let (columns, rows) = v.pivot_export_table(0, cx).expect("pivot builds");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"east") && names.contains(&"west"),
            "exported columns are the pivoted shape (one column per name), got {names:?}"
        );
        assert!(!rows.is_empty());

        v.result_sets[0].row_count_hint = 500;
        assert!(
            v.pivot_export_table(0, cx).is_none(),
            "incomplete sets must not export a partial pivot"
        );
    });
}

/// Streaming regression: while a query is still running, each `Rows`
/// batch must be appended to the EXISTING grid entity (the O(new-rows)
/// fast path), not trigger a full `GridData` rebuild + entity recreation.
/// Recreating the entity per 256-row batch made streaming O(n²) overall
/// and froze the UI on large result sets.
#[gpui::test]
fn streaming_rows_append_in_place_without_recreating_grid(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..30), cx);
        v
    });
    cx.run_until_parked();

    let first_entity = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").entity_id()
    });

    // Second batch arrives while the query is still running.
    view.update(cx, |v, cx| {
        v.handle_event(rows_batch(30..75), cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        let grid = v.grid_views[0].as_ref().expect("grid still exists");
        assert_eq!(
            grid.entity_id(),
            first_entity,
            "a streaming Rows batch must append in place, not recreate the grid entity"
        );
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.data.rows.len(), 75, "both batches must be in the grid");
        assert_eq!(
            s.display_indices.len(),
            75,
            "display order must cover the appended rows"
        );
        assert_eq!(
            s.data.rows[74][1],
            sqlly_datatable::CellValue::Text("row74".into()),
            "appended cells must carry the streamed values"
        );
    });
}

/// `RowsProgress` is count-only: the displayed total advances but no rows
/// are appended, the grid entity is not rebuilt, and its data is
/// untouched (no append, no refresh).
#[gpui::test]
fn rows_progress_updates_count_without_touching_grid(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();

    let grid_entity = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").entity_id()
    });

    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 5_000,
            },
            cx,
        );
        // Stale/reordered progress must never lower the count.
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 4_000,
            },
            cx,
        );
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert_eq!(v.total_rows(), 5_000, "displayed count follows progress");
        assert_eq!(
            v.result_sets[0].rows.len(),
            100,
            "in-memory rows stay on the result set so reveal can restore raw cells"
        );
        let grid = v.grid_views[0].as_ref().expect("grid still exists");
        assert_eq!(grid.entity_id(), grid_entity, "grid entity not rebuilt");
        let s = grid.read(cx).state.read(cx);
        assert_eq!(s.data.rows.len(), 100, "grid data untouched");
        assert_eq!(v.synced_rows[0], 100, "sync watermark untouched");
    });
}

/// Progress events carry the service result-set `index`; with multiple
/// sets the count must land on the matching slot only.
#[gpui::test]
fn rows_progress_maps_index_to_correct_slot(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..10), cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 1,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 1,
                rows: (0..5).map(|i| vec![CellValue::I64(i)]).collect(),
            },
            cx,
        );
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 1,
                rows_seen: 700,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert_eq!(v.row_count_for_slot(0), 10, "slot 0 unaffected");
        assert_eq!(v.row_count_for_slot(1), 700, "slot 1 got the progress");
        assert_eq!(v.total_rows(), 710);
    });
}

/// The spill manifest's `row_count` is authoritative: it must OVERWRITE a
/// stale higher progress hint, not `max` with it.
#[gpui::test]
fn spill_manifest_total_overwrites_stale_higher_hint(cx: &mut gpui::TestAppContext) {
    let total = 200usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        // Over-running progress (e.g. from a batch counted twice).
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 999,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| assert_eq!(v.total_rows(), 999));

    let manifest_event = QueryEvent::SpillManifest {
        manifest: sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                row_count: total,
                spill_file: spill_file.clone(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: Some(1),
        },
    };
    view.update(cx, |v, cx| {
        v.handle_event(manifest_event, cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, _cx| {
        assert_eq!(
            v.result_sets[0].row_count_hint, total,
            "manifest total replaces the stale hint"
        );
        assert_eq!(v.total_rows(), total);
    });

    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// A burst of count-only progress events must coalesce into a single
/// armed timer and exactly one repaint when it fires.
#[gpui::test]
fn streaming_notify_coalesces_progress_bursts(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();

    let notify_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = notify_count.clone();
    let _subscription = cx.update(|_window, app| {
        app.observe(&view, move |_, _| {
            counter.set(counter.get() + 1);
        })
    });

    view.update(cx, |v, cx| {
        for i in 1..=50usize {
            v.handle_event(
                QueryEvent::RowsProgress {
                    index: 0,
                    rows_seen: i * 100,
                },
                cx,
            );
        }
        assert!(v.streaming_notify_pending, "repaint owed");
        assert!(
            v.streaming_notify_task.is_some(),
            "one timer armed for the whole burst"
        );
    });
    cx.run_until_parked();
    assert_eq!(
        notify_count.get(),
        0,
        "no repaint until the frame timer fires"
    );

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    assert_eq!(notify_count.get(), 1, "burst coalesced to a single notify");
    view.read_with(cx, |v, _cx| {
        assert!(!v.streaming_notify_pending);
        assert!(v.streaming_notify_task.is_none(), "timer disarmed");
        assert_eq!(v.total_rows(), 5_000);
    });
}

/// A burst of `Rows` batches must coalesce too: the append fast path
/// notifies only the GridState (for progressive canvas paint), so the
/// ResultsView itself repaints at most once when the frame timer fires.
#[gpui::test]
fn streaming_notify_coalesces_rows_bursts(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();

    let notify_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = notify_count.clone();
    let _subscription = cx.update(|_window, app| {
        app.observe(&view, move |_, _| {
            counter.set(counter.get() + 1);
        })
    });

    view.update(cx, |v, cx| {
        for i in 0..50i64 {
            v.handle_event(rows_batch(i..i + 1), cx);
        }
        assert!(v.streaming_notify_pending, "repaint owed");
        assert!(
            v.streaming_notify_task.is_some(),
            "one timer armed for the whole burst"
        );
    });
    cx.run_until_parked();
    assert_eq!(
        notify_count.get(),
        0,
        "no view repaint until the frame timer fires"
    );

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();

    assert_eq!(notify_count.get(), 1, "burst coalesced to a single notify");
    view.read_with(cx, |v, _cx| {
        assert_eq!(v.total_rows(), 50);
    });
}

/// Terminal events repaint immediately and clear the pending flag, so an
/// armed frame timer must NOT add a duplicate trailing repaint.
#[gpui::test]
fn terminal_event_clears_pending_streaming_notify(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v
    });
    cx.run_until_parked();

    let notify_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = notify_count.clone();
    let _subscription = cx.update(|_window, app| {
        app.observe(&view, move |_, _| {
            counter.set(counter.get() + 1);
        })
    });

    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 1_000,
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(!v.streaming_notify_pending, "terminal event cleared it");
    });
    cx.run_until_parked();
    assert_eq!(notify_count.get(), 1, "terminal repaint is immediate");

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(20));
    cx.run_until_parked();
    assert_eq!(
        notify_count.get(),
        1,
        "the armed timer's wake must not repaint again"
    );
}

/// Streaming regression: the results grid must PAINT while the query is
/// still running. The old content-area gate rendered a "Query running…"
/// placeholder instead of the grid until the terminal event, so rows only
/// appeared all at once on completion. Painted bounds are the proof the
/// grid reached the screen: they are only set by the widget's paint pass.
#[gpui::test]
fn grid_paints_while_query_is_still_running(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..30), cx);
        // No Completed — the query is still in flight.
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert!(v.is_running(), "query must still be running");
        let s = v.grid_views[0]
            .as_ref()
            .expect("grid entity exists mid-query")
            .read(cx)
            .state
            .read(cx);
        let size = s.bounds.size;
        assert!(
            f32::from(size.width) > 0.0 && f32::from(size.height) > 0.0,
            "grid must have painted (non-zero bounds) while the query is running, got {size:?}"
        );
    });
}

/// Drive a real `ResultsView` through the query-event pipeline, render it
/// in a test window, and simulate a left click inside the first data cell.
/// This exercises the full app integration (ensure_grid_for_slot →
/// SqllyDataTable → nested render tree) and asserts the grid actually
/// records the selection — the behavior the user reports as broken.
#[gpui::test]
fn tabbed_grid_left_click_selects_cell(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..30)
                    .map(|i| vec![CellValue::I64(i), CellValue::String(format!("row{i}"))])
                    .collect(),
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert!(
            v.grid_views[0]
                .as_ref()
                .expect("grid view exists")
                .read(cx)
                .pivot_state()
                .is_some(),
            "every in-memory results grid must expose the native Pivot tab"
        );
    });

    // Pull the geometry the grid actually painted into.
    let geom = view.read_with(cx, |v, cx| {
        v.grid_views.first().and_then(|g| g.as_ref()).map(|g| {
            let s = g.read(cx).state.read(cx);
            (
                s.bounds.origin,
                s.bounds.size,
                s.header_height,
                s.row_height,
            )
        })
    });
    let (origin, size, header_h, row_h) = geom.expect("grid view exists for slot 0");
    assert!(
        f32::from(size.width) > 60.0 && f32::from(size.height) > header_h + row_h,
        "grid must have a non-zero paintable area, got {size:?}"
    );

    // Click inside row 0, column 0 (x past the 50px row-header gutter).
    let x = f32::from(origin.x) + 90.0;
    let y = f32::from(origin.y) + header_h + row_h * 0.5;
    cx.simulate_mouse_down(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    let selection = view.read_with(cx, |v, cx| {
        v.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .selection
            .clone()
    });
    assert_eq!(
        selection,
        sqlly_datatable::Selection::Cell(0, 0),
        "left click in the results grid should select Cell(0,0), got {selection:?}"
    );
}

/// A single expanded stacked result uses the whole result area rather than
/// retaining the multi-result adaptive card height.
#[gpui::test]
fn stacked_single_result_fills_area_and_selects_cell(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.set_layout_mode(LayoutMode::Stacked, cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..500)
                    .map(|i| vec![CellValue::I64(i), CellValue::String(format!("row{i}"))])
                    .collect(),
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();

    let geom = view.read_with(cx, |v, cx| {
        v.grid_views.first().and_then(|g| g.as_ref()).map(|g| {
            let s = g.read(cx).state.read(cx);
            (
                s.bounds.origin,
                s.bounds.size,
                s.header_height,
                s.row_height,
            )
        })
    });
    let (origin, size, header_h, row_h) = geom.expect("grid view exists for slot 0");
    assert!(
        f32::from(size.width) > 60.0 && f32::from(size.height) > header_h + row_h,
        "stacked grid must have a non-zero paintable area, got {size:?}"
    );
    assert!(
        f32::from(size.height) > MAX_CARD_HEIGHT,
        "single stacked grid should exceed the capped multi-result card height, got {size:?}"
    );

    let x = f32::from(origin.x) + 90.0;
    let y = f32::from(origin.y) + header_h + row_h * 0.5;
    cx.simulate_mouse_down(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    let selection = view.read_with(cx, |v, cx| {
        v.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .selection
            .clone()
    });
    assert_eq!(
        selection,
        sqlly_datatable::Selection::Cell(0, 0),
        "left click in the stacked (scrollable) results grid should select \
             Cell(0,0), got {selection:?}"
    );
}

#[gpui::test]
fn stacked_results_render_pane_dividers_between_result_sets(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (_view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.set_layout_mode(LayoutMode::Stacked, cx);
        for index in 0..2 {
            view.handle_event(
                QueryEvent::ResultSet {
                    index,
                    columns: vec![ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    }],
                },
                cx,
            );
        }
        view
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("stacked-result-divider-0").is_some(),
        "stacked result sets should be separated by PaneDivider"
    );
    assert!(
        cx.debug_bounds("stacked-result-divider-1").is_some(),
        "the last result set carries its own bottom divider so it can be \
             resized by dragging"
    );
}

const CANVAS_CELL_SELECTORS: [&str; 5] = [
    "canvas-cell-0",
    "canvas-cell-1",
    "canvas-cell-2",
    "canvas-cell-3",
    "canvas-cell-4",
];

/// Canvas-mode results view fed `count` one-column result sets for `sql`.
fn canvas_results_window<'a>(
    cx: &'a mut gpui::TestAppContext,
    sql: &'static str,
    count: usize,
    size: gpui::Size<gpui::Pixels>,
) -> &'a mut gpui::VisualTestContext {
    ensure_components_initialized(cx);
    let (_view, cx) = cx.add_window_view(move |_window, cx| {
        let mut view = ResultsView::new(cx);
        view.set_layout_mode(LayoutMode::Canvas, cx);
        view.set_query_sql(sql, cx);
        for index in 0..count {
            view.handle_event(
                QueryEvent::ResultSet {
                    index,
                    columns: vec![ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    }],
                },
                cx,
            );
        }
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view
    });
    cx.simulate_resize(size);
    cx.run_until_parked();
    cx
}

fn assert_px_close(actual: gpui::Pixels, expected: f32, what: &str) {
    let actual = f32::from(actual);
    assert!(
        (actual - expected).abs() <= 1.5,
        "{what}: expected ~{expected}, got {actual}"
    );
}

#[gpui::test]
fn canvas_cells_split_the_pane_evenly_at_any_size_and_result_count(cx: &mut gpui::TestAppContext) {
    const SQL: &str = "-- sqlly 2x2\nSELECT 1;\nSELECT 2;\nSELECT 3;\nSELECT 4;\n";
    for size in [
        gpui::size(px(1200.0), px(800.0)),
        gpui::size(px(640.0), px(420.0)),
    ] {
        for count in 1..=5usize {
            let cx = canvas_results_window(cx, SQL, count, size);
            let board = cx
                .debug_bounds("canvas-results")
                .expect("a 2x2 directive renders the canvas board");
            // The board fills the pane edge to edge, not a box pinned right.
            assert_px_close(board.size.width, f32::from(size.width), "board width");
            assert!(
                f32::from(board.size.height) > f32::from(size.height) * 0.6,
                "board should take most of the pane height, got {:?} in {size:?}",
                board.size
            );
            // Two equal tracks per axis, separated by the 1px gap.
            let cell_w = (f32::from(board.size.width) - 1.0) / 2.0;
            let cell_h = (f32::from(board.size.height) - 1.0) / 2.0;
            for (slot, selector) in CANVAS_CELL_SELECTORS.iter().enumerate() {
                let cell = cx.debug_bounds(selector);
                if slot >= count.min(4) {
                    // Unfilled cells stay empty; a fifth set has no cell.
                    assert!(cell.is_none(), "{selector} rendered with {count} sets");
                    continue;
                }
                let cell = cell.unwrap_or_else(|| panic!("{selector} missing ({count} sets)"));
                let (row, col) = (slot / 2, slot % 2);
                let what = format!("{selector} of {count} at {size:?}");
                assert_px_close(cell.size.width, cell_w, &format!("{what} width"));
                assert_px_close(cell.size.height, cell_h, &format!("{what} height"));
                assert_px_close(
                    cell.origin.x,
                    f32::from(board.origin.x) + col as f32 * (cell_w + 1.0),
                    &format!("{what} x"),
                );
                assert_px_close(
                    cell.origin.y,
                    f32::from(board.origin.y) + row as f32 * (cell_h + 1.0),
                    &format!("{what} y"),
                );
            }
        }
    }
}

#[gpui::test]
fn canvas_spanning_placements_cover_their_claimed_cells(cx: &mut gpui::TestAppContext) {
    const SQL: &str = "-- sqlly 2x4\n\
        -- sqlly result: 1|1-4\n\
        SELECT 1;\n\
        -- sqlly result: 2|1-2\n\
        SELECT 2;\n";
    let cx = canvas_results_window(cx, SQL, 2, gpui::size(px(1201.0), px(700.0)));
    let board = cx.debug_bounds("canvas-results").expect("canvas board");
    let banner = cx.debug_bounds("canvas-cell-0").expect("banner cell");
    let half = cx.debug_bounds("canvas-cell-1").expect("bottom-left cell");
    let track_w = (f32::from(board.size.width) - 3.0) / 4.0;
    let track_h = (f32::from(board.size.height) - 1.0) / 2.0;
    assert_px_close(
        banner.size.width,
        f32::from(board.size.width),
        "banner width",
    );
    assert_px_close(banner.size.height, track_h, "banner height");
    assert_px_close(half.size.width, track_w * 2.0 + 1.0, "half-row width");
    assert_px_close(half.origin.x, f32::from(board.origin.x), "half-row x");
    assert_px_close(
        half.origin.y,
        f32::from(board.origin.y) + track_h + 1.0,
        "half-row y",
    );
}

#[gpui::test]
fn canvas_placement_without_a_result_set_stays_empty(cx: &mut gpui::TestAppContext) {
    // Two placed statements, but only the first result set has arrived: the
    // second placement must be an empty cell, not a second copy of set 1.
    const SQL: &str = "-- sqlly 2x4\n\
        -- sqlly result: 1|1-4\n\
        SELECT 1;\n\
        -- sqlly result: 2|1-2\n\
        SELECT 2;\n";
    let cx = canvas_results_window(cx, SQL, 1, gpui::size(px(1201.0), px(700.0)));
    let board = cx.debug_bounds("canvas-results").expect("canvas board");
    let only = cx.debug_bounds("canvas-cell-0").expect("first result set");
    assert_px_close(
        only.origin.y,
        f32::from(board.origin.y),
        "set 1 stays on the top row",
    );
    assert_px_close(only.size.width, f32::from(board.size.width), "set 1 width");
    assert!(cx.debug_bounds("canvas-cell-1").is_none());
}

#[gpui::test]
fn canvas_ignores_an_oversized_board_and_stacks(cx: &mut gpui::TestAppContext) {
    // The parser flags a 9x2 board as ignored; it must not still render a
    // 9-row board of slivers.
    const SQL: &str = "-- sqlly 9x2\nSELECT 1;\nSELECT 2;\n";
    let cx = canvas_results_window(cx, SQL, 2, gpui::size(px(900.0), px(600.0)));
    assert!(cx.debug_bounds("canvas-results").is_none());
    assert!(cx.debug_bounds("stacked-result-divider-0").is_some());
}

#[gpui::test]
fn canvas_reports_result_sets_with_no_open_cell(cx: &mut gpui::TestAppContext) {
    const SQL: &str = "-- sqlly 1x2\nSELECT 1;\nSELECT 2;\n";
    let size = gpui::size(px(900.0), px(600.0));
    {
        let cx = canvas_results_window(cx, SQL, 2, size);
        assert!(
            cx.debug_bounds("canvas-layout-notes").is_none(),
            "a board that fits every set shows no layout notes"
        );
    }
    {
        // An overlapping placement is visible in the SQL, so the editor
        // squiggles it; the set is auto-placed and the board stays quiet.
        const OVERLAP: &str = "-- sqlly 1x2\n-- sqlly result: 1|1\nSELECT 1;\n\
                               -- sqlly result: 1|1\nSELECT 2;\n";
        let cx = canvas_results_window(cx, OVERLAP, 2, size);
        assert!(
            cx.debug_bounds("canvas-layout-notes").is_none(),
            "SQL-visible layout problems belong to the editor, not the board"
        );
    }
    let cx = canvas_results_window(cx, SQL, 3, size);
    let notes = cx
        .debug_bounds("canvas-layout-notes")
        .expect("a third set on a 1x2 board must be reported, not silently dropped");
    let board = cx.debug_bounds("canvas-results").expect("canvas board");
    assert!(
        board.origin.y + board.size.height <= notes.origin.y + px(0.5),
        "the notes sit below the board, not over the grids"
    );
}

#[gpui::test]
fn stacked_result_toolbar_clears_grouping_filters_and_sort(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.set_layout_mode(LayoutMode::Stacked, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view
    });
    cx.run_until_parked();

    let state = view.read_with(&*cx, |view, app| {
        view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(app)
            .state
            .clone()
    });
    cx.update(|_window, app| {
        state.update(app, |state, _| {
            state.set_grouped_column(Some(0));
            state.filters[0].values = Some(Default::default());
            state.sort = Some((0, sqlly_datatable::SortDirection::Descending));
            state.recompute();
        });
    });

    cx.update(|_window, app| {
        view.update(app, |view, cx| view.clear_slot_grouping(0, cx));
    });
    state.read_with(&*cx, |state, _| {
        assert_eq!(state.grouped_column(), None);
        assert!(state.filters[0].is_active());
        assert!(state.sort.is_some());
    });

    cx.update(|_window, app| {
        view.update(app, |view, cx| view.clear_slot_filters(0, cx));
    });
    state.read_with(&*cx, |state, _| {
        assert!(state.filters.iter().all(|filter| !filter.is_active()));
        assert!(state.sort.is_some());
    });

    cx.update(|_window, app| {
        view.update(app, |view, cx| view.clear_slot_sort(0, cx));
    });
    state.read_with(&*cx, |state, _| assert!(state.sort.is_none()));

    // The controls sit in the card's own header, not the results tab row.
    cx.run_until_parked();
    for id in [
        "results-clear-grouping-0",
        "results-filter-builder-0",
        "results-clear-filter-0",
        "results-clear-sort-0",
    ] {
        assert!(
            cx.debug_bounds(id).is_some(),
            "{id} should render in result 0's card header"
        );
    }
}

/// Same real `ResultsView`, but right-click a column header and assert the
/// provider-driven context menu opens.
#[gpui::test]
fn tabbed_grid_right_click_header_opens_menu(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..10).map(|i| vec![CellValue::I64(i)]).collect(),
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();

    let (origin, header_h) = view
        .read_with(cx, |v, cx| {
            v.grid_views.first().and_then(|g| g.as_ref()).map(|g| {
                let s = g.read(cx).state.read(cx);
                (s.bounds.origin, s.header_height)
            })
        })
        .expect("grid view exists");

    let x = f32::from(origin.x) + 90.0;
    let y = f32::from(origin.y) + header_h * 0.5;
    cx.simulate_mouse_down(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    let has_menu = view.read_with(cx, |v, cx| {
        v.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .context_menu
            .is_some()
    });
    assert!(
        has_menu,
        "right-click on a results column header should open the context menu"
    );
}

#[test]
fn export_format_all_contains_expected_variants() {
    let all = ExportFormat::all();
    assert!(all.contains(&ExportFormat::Csv));
    assert!(all.contains(&ExportFormat::Json));
    assert!(all.contains(&ExportFormat::Insert));
}

#[test]
fn export_format_labels() {
    assert_eq!(ExportFormat::Csv.label(), "CSV");
    assert_eq!(ExportFormat::Tsv.label(), "TSV");
    assert_eq!(ExportFormat::Json.label(), "JSON");
}

#[test]
fn toolbar_model_languages_follow_language_prefs() {
    use crate::model_codegen::ModelLanguage;
    use crate::result_export_code::CodeLanguage;
    // Defaults: no programming languages enabled, so no model rows either.
    let mut prefs = crate::preferences::ExportPreferences::default();
    assert!(toolbar_model_languages(&prefs).is_empty());
    // TypeScript's switch brings TypeScript, Zod, and Prisma; Python's
    // brings the dataclass and SQLAlchemy; nothing else appears.
    prefs.programming_enabled = true;
    for lang in CodeLanguage::all() {
        let want = matches!(lang, CodeLanguage::TypeScript | CodeLanguage::Python);
        if prefs.language_flag(*lang) != want {
            prefs.toggle_language(*lang);
        }
    }
    let langs = toolbar_model_languages(&prefs);
    assert_eq!(
        langs,
        vec![
            ModelLanguage::TypeScript,
            ModelLanguage::Zod,
            ModelLanguage::Prisma,
            ModelLanguage::Python,
            ModelLanguage::SqlAlchemy,
        ]
    );
}

#[test]
fn toolbar_export_formats_default_prefs() {
    // Defaults: all twelve standard formats, no programming formats.
    let prefs = crate::preferences::ExportPreferences::default();
    let (standard, programming) = toolbar_export_formats(&prefs);
    assert_eq!(standard, ExportFormat::all().to_vec());
    assert!(programming.is_empty());
}

#[test]
fn toolbar_export_formats_respect_toggles() {
    let prefs = crate::preferences::ExportPreferences {
        csv: false,
        xlsx: false,
        programming_enabled: true,
        rust: false,
        ..Default::default()
    };
    let (standard, programming) = toolbar_export_formats(&prefs);
    assert!(!standard.contains(&ExportFormat::Csv));
    assert!(!standard.contains(&ExportFormat::Xlsx));
    assert!(standard.contains(&ExportFormat::Json));
    assert!(!programming.contains(&ExportFormat::Rust));
    assert!(programming.contains(&ExportFormat::CSharp));
    assert_eq!(programming.len(), 10);
}

#[test]
fn toolbar_export_formats_all_disabled_is_empty() {
    let mut prefs = crate::preferences::ExportPreferences::default();
    for fmt in ExportFormat::all() {
        if prefs.standard_enabled(*fmt) {
            prefs.toggle_standard(*fmt);
        }
    }
    let (standard, programming) = toolbar_export_formats(&prefs);
    assert!(standard.is_empty());
    assert!(programming.is_empty());
}

#[test]
fn result_set_default_is_empty() {
    let rs = ResultSet::default();
    assert!(rs.columns.is_empty());
    assert!(rs.rows.is_empty());
    assert!(rs.rows_affected.is_none());
}

#[test]
fn total_rows_sums_across_sets() {
    let view = ResultsView {
        result_sets: vec![
            ResultSet {
                index: 0,
                columns: vec![],
                rows: vec![vec![], vec![]],
                rows_affected: None,
                row_count_hint: 2,
                ..Default::default()
            },
            ResultSet {
                index: 1,
                columns: vec![],
                rows: vec![vec![]],
                rows_affected: None,
                row_count_hint: 1,
                ..Default::default()
            },
        ],
        ..bare_results_view()
    };

    assert_eq!(view.total_rows(), 3);
}

/// `row_count_for_slot` semantics per backing kind: in-memory slots use
/// `rows.len()`, progress-only slots (payload suppression) use the larger
/// hint, and `total_rows` sums the logical counts — not `rows.len()`.
#[test]
fn total_rows_uses_logical_counts_for_progress_only_sets() {
    let view = ResultsView {
        result_sets: vec![
            ResultSet {
                index: 0,
                rows: vec![vec![], vec![]],
                row_count_hint: 2,
                ..Default::default()
            },
            // Payload forwarding stopped at 1 resident row; RowsProgress
            // pushed the hint to 500.
            ResultSet {
                index: 1,
                rows: vec![vec![]],
                row_count_hint: 500,
                ..Default::default()
            },
        ],
        ..bare_results_view()
    };
    assert_eq!(view.row_count_for_slot(0), 2);
    assert_eq!(view.row_count_for_slot(1), 500);
    assert_eq!(view.row_count_for_slot(9), 0, "missing slot counts as 0");
    assert_eq!(view.total_rows(), 502);
}

/// Spill-backed slots must report the backing's total — `rs.rows` is
/// empty for them, and the hint is irrelevant once the file is installed.
#[test]
fn row_count_for_slot_prefers_spill_backing_total() {
    let spill_file = write_test_spill(10);
    let pager =
        crate::spill_pager::SpillRowPager::open(&spill_file).expect("pager opens spill file");
    let view = ResultsView {
        result_sets: vec![ResultSet {
            index: 0,
            row_count_hint: 3,
            ..Default::default()
        }],
        spill_backings: vec![Some(SpillBacking {
            pager: std::cell::RefCell::new(pager),
            total: 10,
            path: spill_file.clone(),
            row_source: std::cell::RefCell::new(None),
            _owned_file: None,
        })],
        ..bare_results_view()
    };
    assert_eq!(view.row_count_for_slot(0), 10);
    assert_eq!(view.total_rows(), 10);
    drop(view);
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// The Messages pane is the run's log: when it started, what the server said,
/// how many rows each result set produced, and when it finished.
#[test]
fn messages_pane_reports_row_counts_server_messages_and_run_clock() {
    let started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_757_520_221);
    let completed = started + std::time::Duration::from_secs(2);
    let view = ResultsView {
        run_started_wall: Some(started),
        completed_wall: Some(completed),
        elapsed_ms: Some(12_128),
        messages: vec!["Changed database context to 'Accounting'.".to_string()],
        result_sets: vec![
            ResultSet {
                index: 0,
                rows: vec![vec![], vec![], vec![]],
                row_count_hint: 3,
                ..Default::default()
            },
            ResultSet {
                index: 1,
                rows_affected: Some(5),
                ..Default::default()
            },
        ],
        ..bare_results_view()
    };

    let body = view.messages_pane_text();
    let lines: Vec<String> = body.lines().map(str::to_string).collect();
    let fmt = ResultFormattingPreferences::default();
    assert_eq!(
        lines,
        vec![
            format!("[{}] Query started", format_run_stamp(started)),
            String::new(),
            String::new(),
            "Changed database context to 'Accounting'.".to_string(),
            format!("Result 1: {} row(s)", fmt.format_integer(3)),
            format!("Result 2: {} row(s) affected", fmt.format_integer(5)),
            String::new(),
            String::new(),
            format!(
                "[{}] Completed in {} ms",
                format_run_stamp(completed),
                fmt.format_integer(12_128)
            ),
        ]
    );
    // The default formatting preferences group thousands, so a long run's
    // elapsed time reads with a separator.
    assert!(body.contains("12,128 ms"), "{body}");
}

/// Before the first run there is nothing to report — no stray clock line.
#[test]
fn messages_pane_is_empty_before_any_run() {
    assert_eq!(bare_results_view().messages_pane_text(), "");
}

/// A SELECT can carry a server DONE count in `rows_affected`; it must not
/// mask the logical row count. "affected" wording is only for sets that
/// produced no rows (DML).
#[test]
fn rows_affected_does_not_mask_select_row_count() {
    let view = ResultsView {
        result_sets: vec![
            ResultSet {
                index: 0,
                rows: vec![vec![], vec![], vec![]],
                rows_affected: Some(3),
                row_count_hint: 3,
                ..Default::default()
            },
            ResultSet {
                index: 1,
                rows_affected: Some(5),
                ..Default::default()
            },
        ],
        ..bare_results_view()
    };
    let fmt = ResultFormattingPreferences::default();
    assert_eq!(
        view.result_count_label(0, &fmt),
        format!("Result 1: {} row(s)", fmt.format_integer(3))
    );
    assert_eq!(
        view.result_count_label(1, &fmt),
        format!("Result 2: {} row(s) affected", fmt.format_integer(5))
    );
}

#[test]
fn selection_column_and_rows_none_selection() {
    let _rs = ResultSet::default();
    let result = selection_column_and_rows(&sqlly_datatable::Selection::None, 0);
    assert!(result.is_none());
}

#[test]
fn selection_column_and_rows_column_selection() {
    let rs = ResultSet {
        index: 0,
        columns: vec![ResultColumn {
            name: "x".into(),
            data_type: "int".into(),
            nullable: true,
        }],
        rows: vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]],
        rows_affected: None,
        row_count_hint: 2,
        ..Default::default()
    };
    let (col, range) =
        selection_column_and_rows(&sqlly_datatable::Selection::Column(0), rs.rows.len()).unwrap();
    assert_eq!(col, 0);
    assert_eq!(range, 0..2);
}

#[test]
fn selection_column_and_rows_cell_selection() {
    let rs = ResultSet {
        index: 0,
        columns: vec![ResultColumn {
            name: "x".into(),
            data_type: "int".into(),
            nullable: true,
        }],
        rows: vec![vec![CellValue::I64(1)]],
        rows_affected: None,
        row_count_hint: 1,
        ..Default::default()
    };
    let (col, range) =
        selection_column_and_rows(&sqlly_datatable::Selection::Cell(0, 0), rs.rows.len()).unwrap();
    assert_eq!(col, 0);
    assert_eq!(range, 0..1);
}

#[test]
fn selection_column_and_rows_row_selection_returns_none() {
    let rs = ResultSet::default();
    let result = selection_column_and_rows(&sqlly_datatable::Selection::Row(0), rs.rows.len());
    assert!(result.is_none());
}

#[test]
fn result_set_label_default() {
    let view = ResultsView {
        result_sets: vec![ResultSet::default()],
        ..bare_results_view()
    };
    assert_eq!(view.result_set_label(0), "Results 1");
}

#[test]
fn result_set_label_from_names() {
    let mut names = BTreeMap::new();
    names.insert(0, "Customers".to_string());
    names.insert(1, "Orders".to_string());
    let view = ResultsView {
        result_sets: vec![ResultSet::default(), ResultSet::default()],
        result_set_names: names,
        ..bare_results_view()
    };
    assert_eq!(view.result_set_label(0), "Customers");
    assert_eq!(view.result_set_label(1), "Orders");
    // Missing index falls back to default.
    assert_eq!(view.result_set_label(2), "Results 3");
}

#[test]
fn layout_mode_default_is_tabbed() {
    assert_eq!(LayoutMode::default(), LayoutMode::Tabbed);
}

#[test]
fn adaptive_card_grid_height_clamps_to_min() {
    let h = adaptive_card_grid_height(0);
    assert!((h - (MIN_CARD_HEIGHT - CARD_HEADER_ALLOWANCE - CARD_FOOTER_ALLOWANCE)).abs() < 0.01);
    let h1 = adaptive_card_grid_height(1);
    assert_eq!(h, h1);
}

#[test]
fn adaptive_card_grid_height_scales_with_rows() {
    let h5 = adaptive_card_grid_height(5);
    let h10 = adaptive_card_grid_height(10);
    assert!(h10 > h5);
    let expected_10 = 24.0 + 10.0 * 22.0 + 24.0 + 16.0 - 24.0 - 24.0;
    assert!((h10 - expected_10).abs() < 0.01);
}

#[test]
fn lines_directive_sizes_the_card_for_that_many_rows() {
    // `lines: 10` → header + (10 data rows + the column-header row) + footer
    // + gaps, and it is not clamped like the adaptive height.
    // Sized with the grid's real row and header heights, so exactly `lines`
    // rows show.
    let metrics = GridDensity::Normal.metrics();
    let h10 = card_total_height_for_lines(10, metrics);
    let expected = CARD_HEADER_ALLOWANCE
        + VIEW_TAB_STRIP_ALLOWANCE
        + metrics.header_height
        + 10.0 * metrics.row_height
        + CARD_FOOTER_ALLOWANCE
        + CARD_GAP * 2.0;
    assert!((h10 - expected).abs() < 0.01);
    assert!(card_total_height_for_lines(100, metrics) > MAX_CARD_HEIGHT);
    assert_eq!(
        card_total_height_for_lines(0, metrics),
        card_total_height_for_lines(1, metrics)
    );
}

#[gpui::test]
fn layout_directive_sets_the_layout_mode_on_run(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let seen: std::rc::Rc<std::cell::RefCell<Vec<LayoutMode>>> = Default::default();
    let sink = seen.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::LayoutModeChanged(mode) = event {
                sink.borrow_mut().push(*mode);
            }
        })
        .detach();
    });
    view.update(cx, |v, cx| {
        v.set_query_sql("-- sqlly layout: stacked\n-- sqlly lines: 8\n-- sqlly lines 2: 20\nSELECT 1;\nSELECT 2;", cx);
    });
    view.read_with(cx, |v, _| {
        assert_eq!(v.layout_mode, LayoutMode::Stacked);
        assert_eq!(v.layout_directives.lines_for(0), Some(8));
        assert_eq!(v.layout_directives.lines_for(1), Some(20));
    });
    assert_eq!(*seen.borrow(), vec![LayoutMode::Stacked]);
    // A script without a layout directive keeps the mode the user chose.
    view.update(cx, |v, cx| v.set_query_sql("SELECT 1", cx));
    view.read_with(cx, |v, _| {
        assert_eq!(v.layout_mode, LayoutMode::Stacked);
        assert_eq!(v.layout_directives.lines_for(0), None);
    });
    assert_eq!(seen.borrow().len(), 1);
}

#[test]
fn adaptive_card_grid_height_clamps_to_max() {
    let h20 = adaptive_card_grid_height(20);
    let h100 = adaptive_card_grid_height(100);
    assert_eq!(h20, h100);
    let expected_max = MAX_CARD_HEIGHT - CARD_HEADER_ALLOWANCE - CARD_FOOTER_ALLOWANCE;
    assert!((h20 - expected_max).abs() < 0.01);
}

#[test]
fn stacked_pane_divider_resizes_only_card_above() {
    let resize = StackedResize {
        slot: 0,
        start_y: 100.0,
        start_height: 240.0,
    };
    assert_eq!(stacked_resize_height(resize, 140.0), 280.0);
    assert_eq!(stacked_resize_height(resize, 60.0), 200.0);
}

#[test]
fn stacked_pane_divider_preserves_minimum_result_chrome() {
    let resize = StackedResize {
        slot: 0,
        start_y: 100.0,
        start_height: 60.0,
    };
    assert_eq!(
        stacked_resize_height(resize, 0.0),
        MIN_RESIZABLE_CARD_HEIGHT
    );
}

#[test]
fn diff_direction_default_is_in_previous() {
    assert_eq!(DiffDirection::default(), DiffDirection::InPrevious);
}

#[test]
fn diff_direction_equality() {
    assert_ne!(DiffDirection::InPrevious, DiffDirection::InCurrent);
    assert_eq!(DiffDirection::InPrevious, DiffDirection::InPrevious);
    assert_eq!(DiffDirection::InCurrent, DiffDirection::InCurrent);
}

#[test]
fn result_set_name_edit_updates_label() {
    let mut names = BTreeMap::new();
    names.insert(0, "Original".to_string());
    let view = ResultsView {
        result_sets: vec![ResultSet::default()],
        layout_mode: LayoutMode::Stacked,
        result_set_names: names,
        collapsed_cards: vec![false],
        ..bare_results_view()
    };
    assert_eq!(view.result_set_label(0), "Original");
    // Simulate a name edit by inserting into result_set_names
    let mut updated = view.result_set_names.clone();
    updated.insert(0, "Renamed".to_string());
    let view2 = ResultsView {
        result_set_names: updated,
        ..view
    };
    assert_eq!(view2.result_set_label(0), "Renamed");
}

// On macOS this test is skipped by default: gpui-component 0.5.2's
// `Root::new` installs a macOS accessibility hit-test forwarder gated
// `#[cfg(all(target_os = "macos", not(test)))]`, which stays active when
// the crate is built as a dependency of OUR test build, and gpui's
// `TestWindow` answers `HasWindowHandle::window_handle` with
// `unimplemented!` instead of an error, so constructing a `Root` in a
// test window panics before the test body runs. CI (Linux/Windows) runs
// the test normally; on macOS it can be forced with `-- --ignored` to
// reproduce the upstream panic.
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui-component Root::new accessibility forwarder panics on gpui TestWindow (macOS-only)"
)]
#[gpui::test]
fn result_settings_dialog_saves_name_focus_and_rows(cx: &mut gpui::TestAppContext) {
    // The dialog's fields are gpui-component Inputs. Their render reads the
    // library's global theme, and focus tracking requires the window root to
    // be a `gpui_component::Root` — exactly like the real app.
    ensure_components_initialized(cx);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(ResultsView::new);
        gpui_component::Root::new(view, window, cx)
    });
    let view = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ResultsView>()
            .expect("window root hosts the ResultsView under test")
    });
    let seen: std::rc::Rc<
        std::cell::RefCell<Vec<crate::result_header_actions::ResultSettingsChange>>,
    > = Default::default();
    let sink = seen.clone();
    cx.update(|window, cx| {
        window.activate_window();
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::ResultSettingsRequested(change) = event {
                sink.borrow_mut().push(change.clone());
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        view.layout_mode = LayoutMode::Stacked;
        view.set_query_sql("-- sqlly lines 1: 6\nSELECT 1;", cx);
        view.result_sets.push(ResultSet::default());
        view.grid_views.push(None);
        view.collapsed_cards.push(false);
        view.stacked_card_heights.push(None);
        view.result_set_names.insert(0, "Original name".into());
        view.open_result_settings(0, cx);
    });
    cx.run_until_parked();

    let (name, rows) = view.read_with(&*cx, |view, _| {
        let dialog = view.result_settings.as_ref().expect("dialog is open");
        (
            dialog.name_input().expect("name input built on render"),
            dialog.rows_input().expect("rows input built on render"),
        )
    });
    cx.update(|window, app| {
        use gpui::Focusable as _;
        assert!(name.read(app).focus_handle(app).is_focused(window));
        assert_eq!(
            rows.read(app).value().as_ref(),
            "6",
            "prefilled from the directive"
        );
    });
    view.update_in(cx, |view, window, cx| {
        name.update(cx, |s, cx| s.set_value("Renamed", window, cx));
        rows.update(cx, |s, cx| s.set_value("9", window, cx));
        view.set_result_settings_focus(true, cx);
        view.save_result_settings(cx);
    });
    view.read_with(&*cx, |view, _| {
        assert!(view.result_settings.is_none(), "Save closes the dialog");
        assert_eq!(
            view.result_set_label(0),
            "Original name",
            "nothing is mirrored until the host has written the query"
        );
    });
    // Stand in for the host, which confirms once the editor was edited.
    let requested = seen.borrow().last().cloned().expect("change requested");
    view.update(cx, |view, cx| view.confirm_result_settings(&requested, cx));
    view.read_with(&*cx, |view, _| {
        assert_eq!(view.result_set_label(0), "Renamed");
        assert_eq!(view.layout_directives.focus_slot(), Some(0));
        assert_eq!(view.layout_directives.lines_for(0), Some(9));
    });
    assert_eq!(
        *seen.borrow(),
        vec![crate::result_header_actions::ResultSettingsChange {
            slot: 0,
            name: Some("Renamed".into()),
            focus: Some(true),
            lines: Some(Some(9)),
            comment_out: false,
        }]
    );

    // An out-of-range row count keeps the dialog open with an error.
    view.update(cx, |view, cx| view.open_result_settings(0, cx));
    cx.run_until_parked();
    let rows = view.read_with(&*cx, |view, _| {
        view.result_settings
            .as_ref()
            .and_then(|d| d.rows_input())
            .expect("rows input")
    });
    view.update_in(cx, |view, window, cx| {
        rows.update(cx, |s, cx| s.set_value("9999", window, cx));
        view.save_result_settings(cx);
    });
    view.read_with(&*cx, |view, _| {
        assert!(
            view.result_settings
                .as_ref()
                .is_some_and(|d| d.error().is_some())
        );
    });
    assert_eq!(seen.borrow().len(), 1);
}

#[test]
fn result_settings_change_writes_only_what_changed() {
    use crate::result_header_actions::ResultSettingsChange;
    let dialog = result_settings::ResultSettingsDialog::new_for_test(1, "Results 2", false, "12");
    assert_eq!(
        dialog.change_for("Results 2", "12").unwrap(),
        ResultSettingsChange {
            slot: 1,
            ..Default::default()
        },
        "untouched fields write nothing"
    );
    let change = dialog.change_for("  Orders ", " 20 ").unwrap();
    assert_eq!(change.name.as_deref(), Some("Orders"));
    assert_eq!(change.lines, Some(Some(20)));
    assert_eq!(
        dialog.change_for("", "").unwrap().lines,
        Some(None),
        "cleared → drop"
    );
    assert_eq!(
        dialog.change_for("", "").unwrap().name,
        None,
        "blank name is ignored"
    );
    assert!(dialog.change_for("x", "0").is_err());
    assert!(dialog.change_for("x", "501").is_err());
    assert!(dialog.change_for("x", "abc").is_err());
}

#[gpui::test]
fn focus_directive_keeps_its_tab_selected_as_results_arrive(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.layout_mode = LayoutMode::Tabbed;
        view.set_query_sql("-- sqlly focus: 2\nSELECT 1;\nSELECT 2;\nSELECT 3;", cx);
        for index in 0..3 {
            let QueryEvent::ResultSet { columns, .. } = two_column_result_set() else {
                unreachable!()
            };
            view.handle_event(QueryEvent::ResultSet { index, columns }, cx);
            let expected = if index == 0 { 0 } else { 1 };
            assert_eq!(view.active_tab, expected, "after result {}", index + 1);
        }
    });
    // Without the directive the newest result stays selected, as before.
    view.update(cx, |view, cx| {
        view.start_query(cx);
        view.set_query_sql("SELECT 1;\nSELECT 2;", cx);
        for index in 0..2 {
            let QueryEvent::ResultSet { columns, .. } = two_column_result_set() else {
                unreachable!()
            };
            view.handle_event(QueryEvent::ResultSet { index, columns }, cx);
        }
        assert_eq!(view.active_tab, 1);
    });
}

/// A request only reaches the host; the view mirrors a change once the host
/// confirms it was written. A refused request (snapshot tab, unmappable
/// statement) must not leave the view showing a setting the query lacks.
#[gpui::test]
fn name_menu_requests_reach_the_host_and_update_the_view(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let seen: std::rc::Rc<
        std::cell::RefCell<Vec<crate::result_header_actions::ResultSettingsChange>>,
    > = Default::default();
    let sink = seen.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::ResultSettingsRequested(change) = event {
                sink.borrow_mut().push(change.clone());
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        let change = |focus, lines| crate::result_header_actions::ResultSettingsChange {
            slot: 1,
            focus,
            lines,
            ..Default::default()
        };
        view.request_result_settings(change(Some(true), None), cx);
        assert_eq!(
            view.layout_directives.focus_slot(),
            None,
            "nothing is mirrored until the host has written the query"
        );
        view.confirm_result_settings(&change(Some(true), None), cx);
        assert_eq!(view.layout_directives.focus_slot(), Some(1));
        view.request_result_settings(change(None, Some(Some(12))), cx);
        assert_eq!(view.layout_directives.lines_for(1), None);
        view.confirm_result_settings(&change(None, Some(Some(12))), cx);
        assert_eq!(view.layout_directives.lines_for(1), Some(12));
        view.request_result_settings(change(None, Some(None)), cx);
        view.confirm_result_settings(&change(None, Some(None)), cx);
        assert_eq!(view.layout_directives.lines_for(1), None);
        view.request_result_settings(change(Some(false), None), cx);
        view.confirm_result_settings(&change(Some(false), None), cx);
        assert_eq!(view.layout_directives.focus_slot(), None);
        let named = crate::result_header_actions::ResultSettingsChange {
            slot: 1,
            name: Some("Second".into()),
            ..Default::default()
        };
        view.request_result_settings(named.clone(), cx);
        assert_eq!(view.result_set_names.get(&1), None);
        view.confirm_result_settings(&named, cx);
        assert_eq!(
            view.result_set_names.get(&1).map(String::as_str),
            Some("Second")
        );
    });
    assert_eq!(seen.borrow().len(), 5);
}

#[test]
fn spill_manifest_field_defaults_to_none() {
    let view = bare_results_view();
    assert!(view.spill_manifest.0.is_none());
    assert!(view.diff_grid_previous.is_none());
    assert!(view.diff_grid_current.is_none());
    assert!(view.diff_grid_diff.is_none());
    assert!(view.result_settings.is_none());
    assert!(view.result_name_menu.is_none());
}

/// Streaming a result set in multiple `Rows` batches must populate the grid
/// incrementally: after each batch the grid's `GridData` reflects every row
/// received so far, the `synced_rows` watermark advances, and (with no sort
/// or filter active) the append fast path keeps `display_indices` in lockstep
/// with the data rows. Guards the O(n²)→O(n) refresh fix.
#[gpui::test]
fn streaming_batches_populate_grid_incrementally(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        v
    });
    cx.run_until_parked();

    let grid_rows = |cx: &mut gpui::TestAppContext| -> (usize, usize) {
        view.read_with(cx, |v, cx| {
            let g = v.grid_views[0].as_ref().unwrap();
            let s = g.read(cx).state.read(cx);
            (s.data.rows.len(), s.display_indices.len())
        })
    };

    // First batch of 10 rows.
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..10).map(|i| vec![CellValue::I64(i)]).collect(),
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        grid_rows(cx),
        (10, 10),
        "after first batch grid must show 10 rows"
    );
    view.read_with(cx, |v, _| assert_eq!(v.synced_rows[0], 10));

    // Second batch of 15 more rows — must append, not replace.
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (10..25).map(|i| vec![CellValue::I64(i)]).collect(),
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        grid_rows(cx),
        (25, 25),
        "after second batch grid must show all 25 rows (incremental append)"
    );
    view.read_with(cx, |v, _| assert_eq!(v.synced_rows[0], 25));

    // Spot-check that appended data is correct end-to-end.
    view.read_with(cx, |v, cx| {
        let g = v.grid_views[0].as_ref().unwrap();
        let s = g.read(cx).state.read(cx);
        assert_eq!(s.data.rows[24][0], sqlly_datatable::CellValue::Integer(24));
    });
}

#[gpui::test]
fn pivot_is_locked_until_streaming_query_finishes(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        view
    });

    view.read_with(cx, |view, cx| {
        let table = view.grid_views[0].as_ref().expect("grid").read(cx);
        assert!(
            table.pivot_state().is_some(),
            "Pivot tab must remain installed"
        );
        assert!(
            table.pivot_locked(),
            "Pivot must be locked while rows stream"
        );
        assert_eq!(table.active_tab(), GridTab::Grid);
    });

    view.update(cx, |view, cx| {
        let table = view.grid_views[0].as_ref().expect("grid").clone();
        table.update(cx, |table, cx| table.set_active_tab(GridTab::Pivot, cx));
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });

    view.read_with(cx, |view, cx| {
        let table = view.grid_views[0].as_ref().expect("grid").read(cx);
        assert!(!table.pivot_locked(), "terminal events unlock Pivot");
        assert_eq!(table.active_tab(), GridTab::Grid);
    });
}

// ───────────────────────────────────────────────────────────────────
// Export integration tests
// ───────────────────────────────────────────────────────────────────

use sqlly_datatable::ContextMenuProvider;

fn make_results_view_with_data(
    cx: &mut gpui::TestAppContext,
) -> (gpui::Entity<ResultsView>, &mut gpui::VisualTestContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "varchar".into(),
                        nullable: true,
                    },
                ],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![
                    vec![CellValue::I64(1), CellValue::String("Alice".into())],
                    vec![CellValue::I64(2), CellValue::Null],
                    vec![CellValue::I64(3), CellValue::String("Bob".into())],
                ],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();
    (view, cx)
}

#[gpui::test]
fn copy_selection_default_uses_markdown_when_set(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.set_copy_format(crate::preferences::CopyFormat::Markdown);
        view.copy_selection_default(0, false, cx);
    });
    let text = cx
        .read_from_clipboard()
        .as_ref()
        .and_then(|c| c.text())
        .unwrap_or_default()
        .to_string();
    assert!(
        text.starts_with("| "),
        "expected markdown table, got {text:?}"
    );
}

#[gpui::test]
fn copy_selection_default_guards_spreadsheet_formulas_in_csv(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "note".into(),
                        data_type: "nvarchar".into(),
                        nullable: true,
                    },
                ],
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![
                    vec![CellValue::I64(-1), CellValue::String("=1+1".into())],
                    vec![CellValue::I64(2), CellValue::String("-12.5".into())],
                ],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        // Pinning the copy format also pins the default (guarded) export
        // preferences, so this never reads the developer's real settings.
        view.set_copy_format(crate::preferences::CopyFormat::Csv);
        view.copy_selection_default(0, true, cx);
    });
    let text = cx
        .read_from_clipboard()
        .as_ref()
        .and_then(|c| c.text())
        .unwrap_or_default()
        .to_string();
    assert_eq!(text, "id,note\n-1,\"'=1+1\"\n2,-12.5");
}

#[gpui::test]
fn tabbed_grid_right_click_data_cell_opens_menu(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let (origin, header_h, row_h) = view
        .read_with(cx, |v, cx| {
            v.grid_views.first().and_then(|g| g.as_ref()).map(|g| {
                let s = g.read(cx).state.read(cx);
                (s.bounds.origin, s.header_height, s.row_height)
            })
        })
        .expect("grid view exists");

    // Right-click on the first data cell (below the header).
    let x = f32::from(origin.x) + 90.0;
    let y = f32::from(origin.y) + header_h + row_h * 0.5;
    cx.simulate_mouse_down(
        gpui::point(px(x), px(y)),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    let has_menu = view.read_with(cx, |v, cx| {
        v.grid_views[0]
            .as_ref()
            .unwrap()
            .read(cx)
            .state
            .read(cx)
            .context_menu
            .is_some()
    });
    assert!(
        has_menu,
        "right-click on a results data cell should open the context menu"
    );
}

#[gpui::test]
fn grid_export_text_formats_via_export_string(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let expected_columns = vec![
        ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        },
        ResultColumn {
            name: "name".into(),
            data_type: "varchar".into(),
            nullable: true,
        },
    ];
    let expected_rows = vec![
        vec![CellValue::I64(1), CellValue::String("Alice".into())],
        vec![CellValue::I64(2), CellValue::Null],
        vec![CellValue::I64(3), CellValue::String("Bob".into())],
    ];

    let text_formats = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::Markdown,
        ExportFormat::Html,
        ExportFormat::Xml,
        ExportFormat::Insert,
        ExportFormat::TempTable,
        ExportFormat::SqlValues,
    ];

    for format in text_formats {
        let actual = view.read_with(cx, |v, cx| v.export_string(format, cx));
        let actual = actual.unwrap_or_else(|| {
            panic!("export_string({format:?}) should return Some for text formats")
        });
        let expected = export_text_for_format(
            format,
            sqlly_lang::sqlmodel::DatabaseType::default(),
            &expected_columns,
            &expected_rows,
        )
        .unwrap_or_else(|| panic!("export_text_for_format({format:?}) should return Some"));
        assert_eq!(
            actual, expected,
            "export_string({format:?}) output mismatch"
        );
    }
}

#[gpui::test]
fn grid_export_xlsx_bytes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let expected_columns = vec![
        ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        },
        ResultColumn {
            name: "name".into(),
            data_type: "varchar".into(),
            nullable: true,
        },
    ];
    let expected_rows = vec![
        vec![CellValue::I64(1), CellValue::String("Alice".into())],
        vec![CellValue::I64(2), CellValue::Null],
        vec![CellValue::I64(3), CellValue::String("Bob".into())],
    ];

    // XLSX is binary — export_string returns None, export_bytes returns the bytes.
    let text_result = view.read_with(cx, |v, cx| v.export_string(ExportFormat::Xlsx, cx));
    assert!(
        text_result.is_none(),
        "XLSX should return None from export_string"
    );

    let bytes = view
        .read_with(cx, |v, cx| v.export_bytes(ExportFormat::Xlsx, cx))
        .expect("export_bytes should return Some for XLSX");
    // XLSX from a grid is the grid workbook (formats, widths, pivot sheet).
    let expected_bytes = view.read_with(cx, |v, cx| {
        v.excel_workbook(0, &expected_columns, &expected_rows, cx)
    });
    assert_eq!(bytes, expected_bytes, "export_bytes(XLSX) output mismatch");
    // Verify it's a valid ZIP (PK signature).
    assert!(bytes.starts_with(&0x0403_4b50u32.to_le_bytes()));
}

#[gpui::test]
fn grid_context_menu_provider_export_items_present_for_cell(cx: &mut gpui::TestAppContext) {
    let (view, _cx) = make_results_view_with_data(cx);

    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak, None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    let request = sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::Cell {
            display_row_index: 0,
            source_row_index: 0,
            column_index: 0,
        },
        None,
        vec![vec![sqlly_datatable::CellValue::Integer(1)]],
        vec![sqlly_datatable::ColumnContext {
            index: 0,
            name: "id".into(),
            kind: sqlly_datatable::ColumnKind::Integer,
        }],
    );

    let items = flat_menu(provider.menu_items(&request));

    // The menu carries exactly ONE export item: "Export…", which opens the
    // consolidated export dialog. Per-format menu entries are gone.
    let export_items: Vec<&sqlly_datatable::ContextMenuItem> = items
        .iter()
        .filter(|item| {
            matches!(item, sqlly_datatable::ContextMenuItem::Action { id, .. }
                if id.starts_with("export_") || id == "export_results_dialog")
        })
        .collect();
    assert_eq!(
        export_items.len(),
        1,
        "cell context menu should carry exactly one export item, got: {items:?}"
    );
    assert!(export_items.iter().any(|item| matches!(
        item,
        sqlly_datatable::ContextMenuItem::Action { id, label }
            if id == "export_results_dialog" && label == "Export…"
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        sqlly_datatable::ContextMenuItem::Action { id, .. } if id == "format_column"
    )));
}

#[gpui::test]
fn grid_context_menu_edit_row_absent_now_in_gutter_icons(cx: &mut gpui::TestAppContext) {
    let (view, _cx) = make_results_view_with_data(cx);
    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak, None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    let rows = vec![vec![sqlly_datatable::CellValue::Integer(1)]];
    let columns = vec![sqlly_datatable::ColumnContext {
        index: 0,
        name: "id".into(),
        kind: sqlly_datatable::ColumnKind::Integer,
    }];
    let has_edit_row = |items: &[sqlly_datatable::ContextMenuItem]| {
        items.iter().any(|item| {
            matches!(
                item,
                sqlly_datatable::ContextMenuItem::Action { id, .. } if id == "edit_row"
            )
        })
    };

    let header_request = sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::RowHeader {
            display_row_index: 0,
            source_row_index: 0,
        },
        None,
        rows.clone(),
        columns.clone(),
    );
    // "Edit Row…" moved out of the context menu into the always-visible
    // eye/pencil icons in the row-header gutter, so it is no longer offered
    // by the row-header context menu either.
    assert!(
        !has_edit_row(&provider.menu_items(&header_request)),
        "row-header context menu must not offer Edit Row (moved to gutter icons)"
    );

    let cell_request = sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::Cell {
            display_row_index: 0,
            source_row_index: 0,
            column_index: 0,
        },
        None,
        rows,
        columns,
    );
    assert!(
        !has_edit_row(&provider.menu_items(&cell_request)),
        "cell context menu must not offer Edit Row…"
    );
}

#[gpui::test]
#[allow(clippy::type_complexity)]
fn open_row_editor_emits_edit_row_request_with_source_row(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let events: std::rc::Rc<std::cell::RefCell<Vec<(usize, Vec<CellValue>, String)>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::EditRowRequested {
                result_set_index,
                row,
                sql,
                ..
            } = event
            {
                seen.borrow_mut()
                    .push((*result_set_index, row.clone(), sql.clone()));
            }
        })
        .detach();
    });

    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id FROM t", cx);
        view.open_row_editor_with(0, 0, crate::row_editor::RowEditorIntent::Edit, cx);
    });

    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "one edit-row request expected");
    let (result_set_index, row, sql) = &recorded[0];
    assert_eq!(*result_set_index, 0);
    assert_eq!(sql, "SELECT id FROM t");
    assert!(!row.is_empty(), "clicked row values must be forwarded");
}

#[gpui::test]
#[allow(clippy::type_complexity)]
fn stage_requested_event_carries_row(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let events: std::rc::Rc<std::cell::RefCell<Vec<(usize, Vec<CellValue>, String)>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::StageRequested {
                result_set_index,
                row,
                sql,
                ..
            } = event
            {
                seen.borrow_mut()
                    .push((*result_set_index, row.clone(), sql.clone()));
            }
        })
        .detach();
    });

    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id FROM t", cx);
        view.open_row_editor_stage_with(0, 0, crate::row_editor::RowEditorIntent::Edit, cx);
    });

    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "one stage request expected");
    let (result_set_index, row, sql) = &recorded[0];
    assert_eq!(*result_set_index, 0);
    assert_eq!(sql, "SELECT id FROM t");
    assert!(!row.is_empty(), "clicked row values must be forwarded");
}

#[gpui::test]
fn stage_delete_many_emits_event(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let events: std::rc::Rc<std::cell::RefCell<Vec<usize>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::StageDeleteManyRequested { rows, .. } = event {
                seen.borrow_mut().push(rows.len());
            }
        })
        .detach();
    });

    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id FROM t", cx);
        view.request_stage_delete_many(0, vec![0, 2], cx);
    });

    let recorded = events.borrow();
    assert_eq!(*recorded, vec![2], "two source rows should be forwarded");
}

#[gpui::test]
fn staged_delete_tints_the_source_row(cx: &mut gpui::TestAppContext) {
    use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};

    let (view, cx) = make_results_view_with_data(cx);
    let session = RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![
            RowEditColumn {
                name: "id".to_string(),
                data_type: "int".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: false,
                is_primary_key: true,
                is_identity: true,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(0),

                enum_values: Vec::new(),
            },
            RowEditColumn {
                name: "name".to_string(),
                data_type: "varchar".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: true,
                is_primary_key: false,
                is_identity: false,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(1),

                enum_values: Vec::new(),
            },
        ],
        original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    };
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Delete {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]),
                original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
            },
            cx,
        );
    });
    view.read_with(cx, |view, cx| {
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid exists")
            .read(cx)
            .state
            .read(cx);
        let tint = state
            .row_backgrounds
            .first()
            .and_then(|c| *c)
            .expect("deleted row should have a background");
        assert!(
            (tint.a - crate::grid_bridge::MARKER_TINT_ALPHA).abs() < 0.01,
            "expected marker tint alpha, got {}",
            tint.a
        );
    });
}

#[gpui::test]
fn staging_on_second_slot_is_refused_while_batch_open(cx: &mut gpui::TestAppContext) {
    use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};

    let (view, cx) = make_results_view_with_data(cx);
    let session = RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![RowEditColumn {
            name: "id".to_string(),
            data_type: "int".to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: false,
            is_primary_key: true,
            is_identity: true,
            is_computed: false,
            is_rowversion: false,
            default_definition: None,
            result_ordinal: Some(0),

            enum_values: Vec::new(),
        }],
        original_row: vec![CellValue::I64(1)],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    };
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::ResultSet {
                index: 1,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::Rows {
                index: 1,
                rows: vec![vec![CellValue::I64(10)]],
            },
            cx,
        );
        view.pending_slot = Some(0);
        view.pending.stage_insert(
            session,
            vec![(
                "id".to_string(),
                crate::pending_changes::CellEdit::Value(CellValue::I64(99)),
            )],
        );
        view.set_query_sql("SELECT id FROM t", cx);
        view.open_row_editor_stage_with(1, 0, crate::row_editor::RowEditorIntent::Edit, cx);
        assert!(
            view.messages.iter().any(|m| m.contains("pending change")),
            "wrong-slot staging should surface a message"
        );
    });
}

fn items_paste_session() -> sqlly_services::row_editing::RowEditSession {
    use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};

    RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![
            RowEditColumn {
                name: "id".to_string(),
                data_type: "int".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: false,
                is_primary_key: true,
                is_identity: true,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(0),

                enum_values: Vec::new(),
            },
            RowEditColumn {
                name: "name".to_string(),
                data_type: "varchar".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: true,
                is_primary_key: false,
                is_identity: false,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(1),

                enum_values: Vec::new(),
            },
        ],
        original_row: vec![CellValue::I64(1), CellValue::String("a".into())],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    }
}

#[gpui::test]
fn apply_paste_stages_rows_and_reports_cell_errors(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = items_paste_session();
    view.update(cx, |view, cx| {
        view.apply_paste(
            0,
            session,
            crate::paste_rows::MappedRows {
                rows: vec![
                    vec![("name".to_string(), CellValue::String("one".into()))],
                    vec![("name".to_string(), CellValue::String("two".into()))],
                ],
                errors: vec![crate::paste_rows::PasteCellError {
                    row: 0,
                    column: "qty".to_string(),
                    text: "x".to_string(),
                    reason: "enter a whole number".to_string(),
                }],
                dropped_columns: 0,
            },
            vec![],
            cx,
        );
        assert_eq!(view.pending.counts().1, 2);
        assert!(
            view.messages.iter().any(|m| m.contains("Paste row 1")),
            "coercion error should surface in Messages, got {:?}",
            view.messages
        );
        assert_eq!(view.marker_offset(0), 1);
        let grid = view.grid_views[0].as_ref().expect("grid");
        let first = grid.read(cx).state.read(cx).data.columns[0].name.clone();
        assert_eq!(first, crate::grid_bridge::MARKER_COLUMN_NAME);
    });
}

#[gpui::test]
fn filled_paste_stages_one_update_per_selected_row(cx: &mut gpui::TestAppContext) {
    // One clipboard value filled down three selected rows: every row is
    // staged and saved, not just the first.
    let (view, cx) = make_results_view_with_data(cx);
    let targets = (1..=3)
        .map(|id| {
            (
                crate::pending_changes::RowKey::new(vec![CellValue::I64(id)]),
                vec![CellValue::I64(id), CellValue::String("old".into())],
                BTreeMap::from([("name".to_string(), CellValue::String("new".into()))]),
            )
        })
        .collect();
    view.update(cx, |view, cx| {
        view.apply_paste(
            0,
            items_paste_session(),
            crate::paste_rows::MappedRows {
                rows: Vec::new(),
                errors: Vec::new(),
                dropped_columns: 0,
            },
            targets,
            cx,
        );
        assert_eq!(view.pending.counts().0, 3);
    });
}

#[gpui::test]
fn marker_column_added_when_pending_nonempty_and_removed_when_cleared(
    cx: &mut gpui::TestAppContext,
) {
    use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};

    let (view, cx) = make_results_view_with_data(cx);
    let session = RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![
            RowEditColumn {
                name: "id".to_string(),
                data_type: "int".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: false,
                is_primary_key: true,
                is_identity: true,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(0),

                enum_values: Vec::new(),
            },
            RowEditColumn {
                name: "name".to_string(),
                data_type: "varchar".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: true,
                is_primary_key: false,
                is_identity: false,
                is_computed: false,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: Some(1),

                enum_values: Vec::new(),
            },
        ],
        original_row: vec![CellValue::I64(1), CellValue::String("a".into())],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    };
    view.update(cx, |view, cx| {
        assert_eq!(view.marker_offset(0), 0);
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Insert {
                values: vec![(
                    "id".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::I64(99)),
                )],
            },
            cx,
        );
        assert_eq!(view.marker_offset(0), 1);
        view.discard_staged(cx);
        assert_eq!(view.marker_offset(0), 0);
    });
}

#[gpui::test]
fn grid_context_menu_provider_export_items_present_for_header(cx: &mut gpui::TestAppContext) {
    let (view, _cx) = make_results_view_with_data(cx);

    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak, None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    let request = sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::ColumnHeader { column_index: 0 },
        None,
        vec![],
        vec![],
    );

    let items = flat_menu(provider.menu_items(&request));

    // Exactly ONE export item on the header menu, opening the dialog.
    let export_items: Vec<&sqlly_datatable::ContextMenuItem> = items
        .iter()
        .filter(|item| {
            matches!(item, sqlly_datatable::ContextMenuItem::Action { id, .. }
                if id.starts_with("export_"))
        })
        .collect();
    assert_eq!(
        export_items.len(),
        1,
        "header context menu should carry exactly one export item, got: {items:?}"
    );
    assert!(export_items.iter().any(|item| matches!(
        item,
        sqlly_datatable::ContextMenuItem::Action { id, label }
            if id == "export_results_dialog" && label == "Export…"
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        sqlly_datatable::ContextMenuItem::Action { id, .. } if id == "format_column"
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        sqlly_datatable::ContextMenuItem::BuiltIn(sqlly_datatable::MenuAction::FreezeToHere)
    )));
}

#[gpui::test]
fn grid_context_menu_provider_export_copies_to_clipboard(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak.clone(), None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    // (format index in the dialog's Standard list, expected clipboard text)
    let test_cases: Vec<(usize, String)> = vec![
        (0, "id,name\n1,Alice\n2,NULL\n3,Bob".to_string()),
        (1, "id\tname\n1\tAlice\n2\t\n3\tBob".to_string()),
        (
            2,
            r#"[{"id":1,"name":"Alice"},{"id":2,"name":null},{"id":3,"name":"Bob"}]"#.to_string(),
        ),
        (
            4,
            "| id | name |\n| --- | --- |\n| 1 | Alice |\n| 2 |  |\n| 3 | Bob |".to_string(),
        ),
        (
            5,
            "<table>\n  <thead>\n    <tr>\n      <th>id</th>\n      <th>name</th>\n    </tr>\n  </thead>\n  <tbody>\n    <tr>\n      <td>1</td>\n      <td>Alice</td>\n    </tr>\n    <tr>\n      <td>2</td>\n      <td></td>\n    </tr>\n    <tr>\n      <td>3</td>\n      <td>Bob</td>\n    </tr>\n  </tbody>\n</table>".to_string(),
        ),
        (
            6,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<results>\n  <row>\n    <column name=\"id\">1</column>\n    <column name=\"name\">Alice</column>\n  </row>\n  <row>\n    <column name=\"id\">2</column>\n    <column name=\"name\" null=\"true\" />\n  </row>\n  <row>\n    <column name=\"id\">3</column>\n    <column name=\"name\">Bob</column>\n  </row>\n</results>".to_string(),
        ),
        (
            7,
            "INSERT INTO [Results] ([id], [name])\nVALUES\n    (1, N'Alice'),\n    (2, NULL),\n    (3, N'Bob');"
                .to_string(),
        ),
        (
            8,
            "CREATE TABLE #Results (\n    [id] int NOT NULL,\n    [name] varchar NULL\n);\n\nINSERT INTO #Results ([id], [name])\nVALUES\n    (1, N'Alice'),\n    (2, NULL),\n    (3, N'Bob');"
                .to_string(),
        ),
        (
            9,
            "SELECT [id], [name]\nFROM (VALUES\n    (1, N'Alice'),\n    (2, NULL),\n    (3, N'Bob')) AS [Results]([id], [name]);"
                .to_string(),
        ),
    ];

    // Use a dummy entity's update to get &mut App — this avoids holding
    // a write lock on ResultsView, which payload resolution needs to read.
    let dummy = cx.new(|_| 0i32);
    dummy.update(cx, |_, cx| {
        let focus = cx.focus_handle();
        let dummy_data = sqlly_datatable::GridData::new(vec![], vec![]).expect("empty grid data");
        let dummy_state = sqlly_datatable::GridState::new(
            dummy_data,
            sqlly_datatable::GridConfig::default(),
            focus,
        );

        // The provider resolves the grid snapshot exactly like the menu
        // action does; the dialog then renders each format from it.
        let payload = provider
            .resolve_export_payload(&dummy_state, cx)
            .expect("payload resolves for the fixture result set");
        let db = view.read(cx).source_db_type();
        let prefs = crate::preferences::ExportPreferences::default();
        let dialog = cx.new(|cx| {
            crate::result_export_dialog::ResultExportDialog::new(
                payload,
                &prefs,
                db,
                weak.clone(),
                cx,
            )
        });

        for (format_index, expected_clipboard) in &test_cases {
            dialog.update(cx, |dialog, cx| {
                dialog.select_format(*format_index, cx);
                dialog.execute(cx);
            });

            let clipboard = cx.read_from_clipboard();
            let clipboard_text = clipboard
                .as_ref()
                .and_then(|c| c.text())
                .unwrap_or_default();
            assert_eq!(
                clipboard_text,
                expected_clipboard.as_str(),
                "clipboard mismatch for export dialog format index: {format_index}"
            );
        }
    });
}

/// The export dialog's binary format (XLSX) routes through the save-file
/// flow: `execute` → `prompt_for_new_path`. We can't simulate the file
/// dialog from outside gpui, but `TestAppContext::did_prompt_for_new_path()`
/// tells us the prompt was requested — proving the dialog reached the save
/// dialog instead of the clipboard.
#[gpui::test]
fn grid_context_menu_provider_export_xlsx_prompts_save(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak.clone(), None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    assert!(
        !cx.did_prompt_for_new_path(),
        "no save prompt should be pending before export"
    );

    let dummy = cx.new(|_| 0i32);
    dummy.update(cx, |_, cx| {
        let focus = cx.focus_handle();
        let dummy_data = sqlly_datatable::GridData::new(vec![], vec![]).expect("empty grid data");
        let dummy_state = sqlly_datatable::GridState::new(
            dummy_data,
            sqlly_datatable::GridConfig::default(),
            focus,
        );
        let payload = provider
            .resolve_export_payload(&dummy_state, cx)
            .expect("payload resolves for the fixture result set");
        let db = view.read(cx).source_db_type();
        let prefs = crate::preferences::ExportPreferences::default();
        let dialog = cx.new(|cx| {
            crate::result_export_dialog::ResultExportDialog::new(
                payload,
                &prefs,
                db,
                weak.clone(),
                cx,
            )
        });
        // Excel (XLSX) is the 11th standard format (index 10). Binary formats
        // force the file destination.
        dialog.update(cx, |dialog, cx| {
            dialog.select_format(10, cx);
            assert_eq!(
                dialog.destination(),
                Some(crate::result_export_dialog::Destination::File),
                "XLSX must force the file destination"
            );
            dialog.execute(cx);
        });
    });

    assert!(
        cx.did_prompt_for_new_path(),
        "exporting XLSX from the dialog should trigger a save-file prompt"
    );
}

/// The header-target menu also carries exactly one Export… item, and the
/// payload it resolves renders the same CSV the cell target produces.
#[gpui::test]
fn grid_context_menu_provider_header_export_copies_to_clipboard(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    let weak = view.downgrade();
    let provider = ResultGridMenuProvider::new(weak.clone(), None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default());

    let request = sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::ColumnHeader { column_index: 0 },
        None,
        vec![],
        vec![],
    );

    let items = flat_menu(provider.menu_items(&request));
    assert_eq!(
        items
            .iter()
            .filter(
                |item| matches!(item, sqlly_datatable::ContextMenuItem::Action { id, .. }
                if id.starts_with("export_"))
            )
            .count(),
        1,
        "header-target menu should carry exactly one export item"
    );

    let dummy = cx.new(|_| 0i32);
    dummy.update(cx, |_, cx| {
        let focus = cx.focus_handle();
        let dummy_data = sqlly_datatable::GridData::new(vec![], vec![]).expect("empty grid data");
        let dummy_state = sqlly_datatable::GridState::new(
            dummy_data,
            sqlly_datatable::GridConfig::default(),
            focus,
        );

        let payload = provider
            .resolve_export_payload(&dummy_state, cx)
            .expect("payload resolves for the fixture result set");
        let db = view.read(cx).source_db_type();
        let prefs = crate::preferences::ExportPreferences::default();
        let dialog = cx.new(|cx| {
            crate::result_export_dialog::ResultExportDialog::new(
                payload,
                &prefs,
                db,
                weak.clone(),
                cx,
            )
        });
        dialog.update(cx, |dialog, cx| dialog.execute(cx));

        let clipboard = cx.read_from_clipboard();
        let clipboard_text = clipboard
            .as_ref()
            .and_then(|c| c.text())
            .unwrap_or_default();
        assert_eq!(
            clipboard_text, "id,name\n1,Alice\n2,NULL\n3,Bob",
            "header-target export dialog (default CSV) should copy the result set to clipboard"
        );
    });
}

/// End-to-end check of the rendered overlay: right-click a data cell, and
/// the menu the grid actually renders must carry exactly one export item —
/// "Export…", which opens the consolidated dialog. The dialog itself runs in
/// its own window, which the gpui test harness cannot open, so the
/// clipboard/file behavior is covered by the dialog-level tests above.
#[gpui::test]
fn grid_rendered_menu_shows_single_export_item(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);

    // Right-click on the first data cell to open the context menu.
    let (origin, header_h, row_h) = view
        .read_with(cx, |v, cx| {
            v.grid_views.first().and_then(|g| g.as_ref()).map(|g| {
                let s = g.read(cx).state.read(cx);
                (s.bounds.origin, s.header_height, s.row_height)
            })
        })
        .expect("grid view exists");

    let click_x = f32::from(origin.x) + 90.0;
    let click_y = f32::from(origin.y) + header_h + row_h * 0.5;
    cx.simulate_mouse_down(
        gpui::point(px(click_x), px(click_y)),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    // Read the menu the grid actually rendered and count export items.
    let export_items: Vec<String> = view
        .read_with(cx, |view, cx| {
            let state = view.grid_views[0].as_ref()?.read(cx).state.read(cx);
            let menu = state.context_menu.as_ref()?;
            Some(
                menu.items
                    .iter()
                    .filter_map(|item| match item {
                        sqlly_datatable::grid::menu::MenuItem::Custom { id, .. }
                            if id.starts_with("export_") || id == "export_results_dialog" =>
                        {
                            Some(id.to_string())
                        }
                        _ => None,
                    })
                    .collect(),
            )
        })
        .expect("rendered menu should be available after right-click");

    assert_eq!(
        export_items,
        vec!["export_results_dialog".to_string()],
        "rendered menu must carry exactly one export item (Export…), got: {export_items:?}"
    );
}

#[test]
fn format_elapsed_zero() {
    assert_eq!(format_elapsed(0), "0:00:00.000");
}

#[test]
fn format_elapsed_subsecond() {
    assert_eq!(format_elapsed(42), "0:00:00.042");
}

#[test]
fn format_elapsed_seconds() {
    assert_eq!(format_elapsed(3_500), "0:00:03.500");
}

#[test]
fn format_elapsed_minutes() {
    assert_eq!(format_elapsed(65_123), "0:01:05.123");
}

#[test]
fn format_elapsed_hours() {
    assert_eq!(format_elapsed(3_725_999), "1:02:05.999");
}

#[test]
fn format_duration_ms_zero() {
    assert_eq!(format_duration_ms(0), "0ms");
}

#[test]
fn format_duration_ms_subsecond() {
    assert_eq!(format_duration_ms(42), "42ms");
}

#[test]
fn format_duration_ms_exactly_one_second() {
    assert_eq!(format_duration_ms(1000), "1.00s");
}

#[test]
fn format_duration_ms_fractional_seconds() {
    assert_eq!(format_duration_ms(1234), "1.23s");
}

#[test]
fn format_duration_ms_large() {
    assert_eq!(format_duration_ms(65_123), "65.12s");
}

#[test]
fn format_count_groups_thousands() {
    assert_eq!(format_count(0), "0");
    assert_eq!(format_count(999), "999");
    assert_eq!(format_count(1_000), "1,000");
    assert_eq!(format_count(1_234_567), "1,234,567");
}

#[test]
fn format_running_elapsed_seconds_then_minutes() {
    assert_eq!(format_running_elapsed(Duration::from_secs(3)), "3s");
    assert_eq!(format_running_elapsed(Duration::from_secs(59)), "59s");
    assert_eq!(format_running_elapsed(Duration::from_secs(67)), "1:07");
    assert_eq!(format_running_elapsed(Duration::from_secs(600)), "10:00");
}

#[gpui::test]
fn elapsed_ticker_stops_after_completed(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.start_query(cx);
        assert!(view.elapsed_ticker.is_some());
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(!view.is_running);
    });
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    view.read_with(cx, |view, _cx| {
        assert!(view.elapsed_ticker.is_none());
    });
}

#[gpui::test]
fn total_count_link_hidden_when_rows_complete(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.perf_prefs.spill_row_threshold = 50_000;
        view.set_query_sql("SELECT id FROM t", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(
            !view.total_count_link_visible(0),
            "complete preview below the spill cap should not offer a total-count link"
        );
    });
}

#[gpui::test]
fn total_count_link_visible_when_hint_exceeds_rows(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.perf_prefs.spill_row_threshold = 50_000;
        view.set_query_sql("SELECT id FROM t", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(
            QueryEvent::RowsProgress {
                index: 0,
                rows_seen: 100_000,
            },
            cx,
        );
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(
            view.total_count_link_visible(0),
            "truncated result above the spill cap should offer a total-count link"
        );
    });
}

#[gpui::test]
fn set_total_row_count_updates_label(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id FROM t", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.set_total_row_count(0, TotalCount::Ready(1_234_567), cx);
        let fmt = ResultFormattingPreferences::default();
        let label = view.result_count_label(0, &fmt);
        assert!(
            label.contains("of 1,234,567"),
            "ready total should appear in the count label, got {label}"
        );
    });
}

#[gpui::test]
fn start_query_clears_total_counts(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_total_row_count(0, TotalCount::Ready(99), cx);
        assert!(!view.total_row_counts.is_empty());
        view.start_query(cx);
        assert!(
            view.total_row_counts.is_empty(),
            "a new run must drop the previous COUNT(*) total"
        );
    });
}

/// Verify that `start_query` → events → `render` populates all four
/// timing fields: `first_response_ms`, `elapsed_ms` (server-side),
/// `response_finished_ms`, and `ui_paint_ms`.
#[gpui::test]
fn query_timing_fields_populated_through_lifecycle(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.start_query(cx);
        v
    });

    // Before any data events: only query_start should be set.
    view.read_with(cx, |v, _| {
        assert!(
            v.query_start.is_some(),
            "query_start should be set by start_query"
        );
        assert!(
            v.first_response_ms.is_none(),
            "first_response_ms should be None before data"
        );
        assert!(
            v.response_finished_ms.is_none(),
            "response_finished_ms should be None before completion"
        );
        assert!(
            v.ui_paint_ms.is_none(),
            "ui_paint_ms should be None before render-after-completion"
        );
    });

    // Send a ResultSet — this is the first server-data event.
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
    });

    // first_response_ms should now be set.
    view.read_with(cx, |v, _| {
        assert!(
            v.first_response_ms.is_some(),
            "first_response_ms should be set after ResultSet"
        );
        assert!(
            v.response_finished_ms.is_none(),
            "response_finished_ms should still be None"
        );
    });

    // Send Completed — this sets elapsed_ms and response_finished_ms.
    view.update(cx, |v, cx| {
        v.handle_event(QueryEvent::Completed { elapsed_ms: 42 }, cx);
    });

    view.read_with(cx, |v, _| {
        assert_eq!(v.elapsed_ms, Some(42), "server elapsed_ms should be set");
        assert!(
            v.response_finished_ms.is_some(),
            "response_finished_ms should be set after Completed"
        );
        assert!(v.completed_at.is_some(), "completed_at should be stamped");
    });

    // Now render — this should set ui_paint_ms.
    cx.run_until_parked();

    view.read_with(cx, |v, _| {
        assert!(
            v.ui_paint_ms.is_some(),
            "ui_paint_ms should be set after render"
        );
        assert!(
            v.wall_clock_ms.is_some(),
            "wall_clock_ms should be set after render"
        );
        assert!(
            v.server_total_ms().is_some(),
            "server_total_ms should be available"
        );
    });
}

/// `first_response_ms` should be set by the first data-carrying event,
/// not by `Started` (which is a local signal, not a server response).
#[gpui::test]
fn first_response_not_set_by_started(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.start_query(cx);
        v
    });

    view.update(cx, |v, cx| {
        v.handle_event(QueryEvent::Started, cx);
    });

    view.read_with(cx, |v, _| {
        assert!(
            v.first_response_ms.is_none(),
            "Started should not set first_response_ms"
        );
    });

    // Now send a RowCount (DML-style query with no result set).
    view.update(cx, |v, cx| {
        v.handle_event(
            QueryEvent::RowCount {
                index: 0,
                rows_affected: 5,
            },
            cx,
        );
    });

    view.read_with(cx, |v, _| {
        assert!(
            v.first_response_ms.is_some(),
            "RowCount should set first_response_ms"
        );
    });
}

/// `clear()` should reset all timing fields to None.
#[gpui::test]
fn clear_resets_timing_fields(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.start_query(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 10 }, cx);
        v
    });

    view.read_with(cx, |v, _| {
        assert!(v.first_response_ms.is_some());
        assert!(v.response_finished_ms.is_some());
        assert!(v.elapsed_ms.is_some());
    });

    view.update(cx, |v, cx| {
        v.clear(cx);
    });

    view.read_with(cx, |v, _| {
        assert!(v.query_start.is_none(), "clear should reset query_start");
        assert!(
            v.first_response_ms.is_none(),
            "clear should reset first_response_ms"
        );
        assert!(
            v.response_finished_ms.is_none(),
            "clear should reset response_finished_ms"
        );
        assert!(v.completed_at.is_none(), "clear should reset completed_at");
        assert!(v.ui_paint_ms.is_none(), "clear should reset ui_paint_ms");
        assert!(
            v.wall_clock_ms.is_none(),
            "clear should reset wall_clock_ms"
        );
        assert!(v.elapsed_ms.is_none(), "clear should reset elapsed_ms");
    });
}

/// `server_total_ms` = `first_response_ms + elapsed_ms`.
/// Returns `None` if either component is missing.
#[gpui::test]
fn server_total_is_first_plus_query(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.first_response_ms = Some(30);
        v.elapsed_ms = Some(100);
        v
    });

    view.read_with(cx, |v, _| {
        assert_eq!(
            v.server_total_ms(),
            Some(130),
            "server_total = first + query"
        );
    });

    // If either component is None, server_total is None.
    view.update(cx, |v, _cx| {
        v.first_response_ms = None;
    });
    view.read_with(cx, |v, _| {
        assert_eq!(
            v.server_total_ms(),
            None,
            "server_total is None without first_response"
        );
    });
}

/// `display_ms` should subtract `estimated_latency_ms` when
/// `subtract_latency` is enabled, clamping at 0.
#[gpui::test]
fn display_ms_subtracts_latency(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.estimated_latency_ms = Some(50);
        v
    });

    view.update(cx, |v, cx| {
        // Without subtract: raw value passes through.
        v.subtract_latency = false;
        assert_eq!(v.display_ms(Some(100)), Some(100));

        // With subtract: 100 - 50 = 50.
        v.subtract_latency = true;
        assert_eq!(v.display_ms(Some(100)), Some(50));

        // Clamp at 0 when latency exceeds raw.
        assert_eq!(v.display_ms(Some(30)), Some(0));

        // None stays None.
        assert_eq!(v.display_ms(None), None);

        // No latency set: passes through even with subtract flag.
        v.estimated_latency_ms = None;
        assert_eq!(v.display_ms(Some(100)), Some(100));
        cx.notify();
    });
}

/// `format_timing` should NOT subtract latency — only `format_first_response`
/// and `display_ms` (used for First) apply latency subtraction.
#[gpui::test]
fn format_timing_does_not_subtract_latency(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.estimated_latency_ms = Some(50);
        v.subtract_latency = true;
        v
    });

    view.read_with(cx, |v, _| {
        // format_timing passes raw value through without subtraction.
        assert_eq!(v.format_timing(Some(100)), Some("100ms".to_string()));
        assert_eq!(v.format_timing(Some(30)), Some("30ms".to_string()));
        assert_eq!(v.format_timing(None), None);
    });
}

/// `format_first_response` should subtract latency when enabled.
#[gpui::test]
fn format_first_response_subtracts_latency(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.estimated_latency_ms = Some(50);
        v.first_response_ms = Some(100);
        v
    });

    view.update(cx, |v, cx| {
        v.subtract_latency = false;
        assert_eq!(v.format_first_response(), Some("100ms".to_string()));

        v.subtract_latency = true;
        assert_eq!(v.format_first_response(), Some("50ms".to_string()));
        cx.notify();
    });
}

/// `toggle_timing_format` should flip between Duration and Elapsed.
#[gpui::test]
fn timing_format_toggle(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));

    view.read_with(cx, |v, _| {
        assert_eq!(
            v.timing_format,
            TimingFormat::Duration,
            "default should be Duration"
        );
    });

    view.update(cx, |v, cx| {
        v.toggle_timing_format(cx);
    });

    view.read_with(cx, |v, _| {
        assert_eq!(
            v.timing_format,
            TimingFormat::Elapsed,
            "should toggle to Elapsed"
        );
    });

    view.update(cx, |v, cx| {
        v.toggle_timing_format(cx);
    });

    view.read_with(cx, |v, _| {
        assert_eq!(
            v.timing_format,
            TimingFormat::Duration,
            "should toggle back to Duration"
        );
    });
}

/// `clear()` should NOT reset `estimated_latency_ms`, `subtract_latency`,
/// or `timing_format` — these are connection-level preferences that
/// persist across queries.
#[gpui::test]
fn clear_preserves_latency_and_format(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.estimated_latency_ms = Some(42);
        v.subtract_latency = true;
        v.timing_format = TimingFormat::Elapsed;
        v
    });

    view.update(cx, |v, cx| {
        v.clear(cx);
    });

    view.read_with(cx, |v, _| {
        assert_eq!(v.estimated_latency_ms, Some(42), "latency should persist");
        assert!(v.subtract_latency, "subtract_latency should persist");
        assert_eq!(
            v.timing_format,
            TimingFormat::Elapsed,
            "timing_format should persist"
        );
    });
}

// --- Messages pane text-field tests ---

#[gpui::test]
fn messages_pane_mirrors_messages_and_preserves_selection(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.messages = vec!["Alpha".into(), "Beta".into()];
        v
    });
    view.update_in(cx, |v, window, cx| {
        v.ensure_messages_input(window, cx);
        v.sync_messages_text(window, cx);
        let input = v.messages_input.clone().expect("input just created");
        assert_eq!(input.read(cx).value(), "Alpha\nBeta");

        // Select "Beta", then let a new message land: the append must not
        // steal the selection.
        input.update(cx, |state, cx| state.set_selected_range(6..10, cx));
        v.messages.push("Error: boom".into());
        v.sync_messages_text(window, cx);
        assert_eq!(input.read(cx).value(), "Alpha\nBeta\nError: boom");
        assert_eq!(input.read(cx).selected_value(), "Beta");
    });
}

#[gpui::test]
fn messages_pane_field_is_read_only(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.messages = vec!["Hello".into()];
        v
    });
    view.update_in(cx, |v, window, cx| {
        v.ensure_messages_input(window, cx);
        v.sync_messages_text(window, cx);
    });
    // A user keystroke lands in the field through the platform text-input
    // path (`set_value` is silent and never reaches the subscription); the
    // revert subscription must put the rendered text straight back.
    let input = view.read_with(cx, |v, _| {
        v.messages_input.clone().expect("input just created")
    });
    input.update_in(cx, |state, window, cx| {
        use gpui::EntityInputHandler as _;
        state.replace_text_in_range(Some(0..0), "edited: ", window, cx);
    });
    cx.run_until_parked();
    input.read_with(cx, |state, _| assert_eq!(state.value(), "Hello"));
}

#[gpui::test]
fn messages_pane_clear_empties_the_field(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.messages = vec!["Hello".into()];
        v
    });
    view.update_in(cx, |v, window, cx| {
        v.ensure_messages_input(window, cx);
        v.sync_messages_text(window, cx);
        v.clear(cx);
        assert!(v.messages.is_empty());
        v.sync_messages_text(window, cx);
        let input = v.messages_input.clone().expect("input just created");
        assert_eq!(input.read(cx).value(), "");
    });
}

/// When a query error arrives via `QueryEvent::Error`, the messages pane
/// must show the actual error text, not "Query cancelled".
#[gpui::test]
fn query_error_populates_messages_with_error_text(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.start_query(cx);
        v.handle_event(
            QueryEvent::Error {
                message: "Incorrect syntax near 'SELET'".to_string(),
            },
            cx,
        );
        v
    });
    view.read_with(cx, |v, _| {
        assert!(
            v.messages.iter().any(|m| m.contains("SELET")),
            "messages should contain the SQL Server error, got: {:?}",
            v.messages
        );
        assert!(
            !v.messages.iter().any(|m| m.contains("cancelled")),
            "messages should NOT contain 'cancelled'"
        );
    });
}

#[test]
fn chrome_tab_labels_are_results_messages_plan_history_ddl() {
    assert_eq!(chrome_tab_caption("Results", 0), "Results");
    assert_eq!(chrome_tab_caption("Results", 1), "Results¹");
    assert_eq!(chrome_tab_caption("Results", 2), "Results²");
    assert_eq!(chrome_tab_caption("Messages", 0), "Messages");
    assert_eq!(chrome_tab_caption("Messages", 3), "Messages³");
    assert_eq!(chrome_tab_caption("Diff", 0), "Diff");
    assert_eq!(chrome_tab_caption("Plan", 0), "Plan");
    assert_eq!(chrome_tab_caption("Plan", 12), "Plan¹²");
    assert!(!chrome_tab_caption("Plan", 1).contains("Execution"));
    assert_eq!(OutputPanelTab::History.label(), "History");
    assert_eq!(OutputPanelTab::Ddl.label(), "DDL");
    assert_eq!(OutputPanelTab::Results.shortcut_index(), Some(1));
    assert_eq!(OutputPanelTab::Messages.shortcut_index(), Some(2));
    assert_eq!(OutputPanelTab::Plan.shortcut_index(), Some(3));
    assert_eq!(OutputPanelTab::History.shortcut_index(), Some(4));
    assert_eq!(OutputPanelTab::Ddl.shortcut_index(), Some(5));
    assert_eq!(OutputPanelTab::Diff.shortcut_index(), Some(6));
    assert_eq!(
        OutputPanelTab::visible_tabs(TabVisibility::chrome_only()),
        OutputPanelTab::core_tabs().to_vec()
    );
    assert!(
        OutputPanelTab::visible_tabs(TabVisibility {
            chrome_visible: false,
            diff: true,
            ..TabVisibility::default()
        })
        .is_empty()
    );
    assert_eq!(OutputPanelTab::Diff.command_id(), "view.selectOutputDiff");
    let with_diff = OutputPanelTab::visible_tabs(TabVisibility {
        diff: true,
        ..TabVisibility::chrome_only()
    });
    assert_eq!(with_diff.last().copied(), Some(OutputPanelTab::Diff));

    // Plan and DDL are conditional: neither shows for a plain run, and each
    // appears only when its own content exists.
    assert!(
        !OutputPanelTab::visible_tabs(TabVisibility::chrome_only()).contains(&OutputPanelTab::Plan)
    );
    assert!(
        !OutputPanelTab::visible_tabs(TabVisibility::chrome_only()).contains(&OutputPanelTab::Ddl)
    );
    let with_plan = OutputPanelTab::visible_tabs(TabVisibility {
        plan: true,
        ..TabVisibility::chrome_only()
    });
    assert_eq!(
        with_plan,
        vec![
            OutputPanelTab::Results,
            OutputPanelTab::Messages,
            OutputPanelTab::Plan,
            OutputPanelTab::History,
        ]
    );
    let with_ddl = OutputPanelTab::visible_tabs(TabVisibility {
        ddl: true,
        ..TabVisibility::chrome_only()
    });
    assert_eq!(with_ddl.last().copied(), Some(OutputPanelTab::Ddl));
}

#[test]
fn empty_state_shortcut_labels_and_keycaps() {
    let rows = empty_state_shortcuts();
    let labels: Vec<String> = rows.iter().map(|row| row.label.clone()).collect();
    assert_eq!(
        labels,
        [
            "Run Query",
            "New Query",
            "Command Palette",
            "Filter Explorer"
        ]
    );
    assert!(rows.iter().all(|row| !row.keycap.is_empty()));
    let run = rows.iter().find(|row| row.label == "Run Query").unwrap();
    let expected_run = crate::keybindings::shortcut_hint("query.execute");
    if expected_run.is_empty() {
        assert_eq!(run.keycap, platform_primary_hint("Enter"));
    } else {
        assert_eq!(run.keycap, expected_run);
    }
    let expected_new = crate::keybindings::shortcut_hint("file.newQuery");
    if expected_new.is_empty() {
        assert_eq!(rows[1].keycap, platform_primary_hint("N"));
    } else {
        assert_eq!(rows[1].keycap, expected_new);
    }
    let expected_palette = crate::keybindings::shortcut_hint("tools.commandPalette");
    if expected_palette.is_empty() {
        assert_eq!(rows[2].keycap, platform_primary_hint("Shift+P"));
    } else {
        assert_eq!(rows[2].keycap, expected_palette);
    }
    assert_eq!(rows[3].keycap, platform_primary_hint("Shift+F"));
    assert!(
        shortcut_uses_keycap_chrome(),
        "empty-state shortcuts draw keycap chips, matching the welcome screen"
    );
}

#[gpui::test]
fn pristine_results_hide_chrome_until_first_run(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.read_with(cx, |view, _cx| {
        assert!(view.is_pristine());
        assert!(!view.results_chrome_visible());
    });
    view.update(cx, |view, cx| {
        view.messages.push("PRINT done".into());
        cx.notify();
    });
    view.read_with(cx, |view, _cx| {
        assert!(!view.is_pristine());
        assert!(view.results_chrome_visible());
    });
}

#[gpui::test]
fn results_view_tab_captions_use_plan_and_message_superscripts(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.read_with(cx, |view, cx| {
        assert_eq!(view.results_chrome_caption(), "Results");
        assert_eq!(view.messages_chrome_caption(), "Messages");
        assert_eq!(view.diff_chrome_caption(), "Diff");
        assert_eq!(view.plan_chrome_caption(cx), "Plan");
    });
    view.update(cx, |view, cx| {
        view.messages.push("PRINT done".into());
        cx.notify();
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(view.messages_chrome_caption(), "Messages¹");
    });
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(
            QueryEvent::ResultSet {
                index: 1,
                columns: vec![ResultColumn {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(view.results_chrome_caption(), "Results²");
    });
}

/// When a query is cancelled (not errored), the messages pane shows
/// "Query cancelled." — the normal cancellation path.
#[gpui::test]
fn query_cancelled_populates_messages_with_cancelled(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.start_query(cx);
        v.handle_event(QueryEvent::Cancelled, cx);
        v
    });
    view.read_with(cx, |v, _| {
        assert!(
            v.messages.iter().any(|m| m.contains("cancelled")),
            "cancelled query should show 'Query cancelled'"
        );
    });
}

#[gpui::test]
fn grid_find_bar_toggles_and_is_safe_without_a_grid(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        // Safe to toggle before any result set / grid exists.
        assert!(view.active_grid_state(cx).is_none());
        assert!(!view.grid_find_visible());
        view.toggle_grid_find(cx);
        assert!(view.grid_find_visible());
        // Driving next/prev with no grid must not panic.
        view.grid_find_next(cx);
        view.grid_find_prev(cx);
        // Toggling again closes it.
        view.toggle_grid_find(cx);
        assert!(!view.grid_find_visible());
    });
}

#[gpui::test]
fn index_label_wins_over_parsed_name_for_that_slot(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql("-- sqlly name 1: Parsed\nSELECT 1", cx);
        view.handle_event(
            QueryEvent::ResultSet {
                index: 1000,
                columns: vec![ResultColumn {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        view.set_index_label(1000, "Dev server".into(), cx);
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(view.result_set_label(0), "Dev server");
    });
}

#[gpui::test]
fn index_label_absent_falls_back_to_results_n(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::ResultSet {
                index: 2000,
                columns: vec![ResultColumn {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(view.result_set_label(0), "Results 1");
    });
}

fn pin_default_masking(view: &mut ResultsView) {
    view.masking_rules = Some(crate::masking_rules::MaskingRules::default());
    view.formatting = Some(ResultFormattingPreferences::default());
}

fn password_result_set() -> QueryEvent {
    QueryEvent::ResultSet {
        index: 0,
        columns: vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "password_hash".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ],
    }
}

fn password_rows() -> QueryEvent {
    QueryEvent::Rows {
        index: 0,
        rows: vec![
            vec![CellValue::I64(1), CellValue::String("xxxxabcd".into())],
            vec![CellValue::I64(2), CellValue::String("yyyyefgh".into())],
        ],
    }
}

fn grid_cell_text(view: &ResultsView, cx: &gpui::App, row: usize, col: usize) -> String {
    let grid = view.grid_views[0].as_ref().expect("grid").clone();
    let state = grid.read(cx).state.read(cx);
    match &state.data.rows[row][col] {
        sqlly_datatable::CellValue::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

#[gpui::test]
fn auto_mask_applies_to_grid_text(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 1), "••••abcd");
        assert_eq!(view.masked_columns(0), vec![1]);
    });
}

#[gpui::test]
fn reveal_column_shows_raw_then_rerun_remasks(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.reveal_column(0, 1, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 1), "xxxxabcd");
        assert!(view.masked_columns(0).is_empty());
    });
    view.update(cx, |view, cx| {
        view.start_query(cx);
        pin_default_masking(view);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 1), "••••abcd");
    });
}

#[gpui::test]
fn reveal_cell_only_affects_that_cell(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.reveal_cell(0, 0, 1, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 1), "xxxxabcd");
        assert_eq!(grid_cell_text(view, cx, 1, 1), "••••efgh");
    });
}

#[gpui::test]
fn copy_of_masked_cell_is_masked(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, _cx| {
        let prefs = view.effective_formatting_for_slot(0);
        let rows = crate::result_cell_format::apply_export_masks(
            &view.result_sets[0].columns,
            view.result_sets[0].rows.clone(),
            &prefs,
        );
        match &rows[0][1] {
            CellValue::String(text) => assert_eq!(text, "••••abcd"),
            other => panic!("expected masked string, got {other:?}"),
        }
    });
}

fn ref_code_result(view: &mut ResultsView, cx: &mut gpui::Context<ResultsView>) {
    view.handle_event(
        QueryEvent::ResultSet {
            index: 0,
            columns: vec![ResultColumn {
                name: "ref_code".into(),
                data_type: "nvarchar(20)".into(),
                nullable: true,
            }],
        },
        cx,
    );
    view.handle_event(
        QueryEvent::Rows {
            index: 0,
            rows: vec![vec![CellValue::String("ABCDEFGH".into())]],
        },
        cx,
    );
    view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
}

#[gpui::test]
fn mask_directive_masks_grid_copy_and_export(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        view.set_query_sql(
            "-- sqlly format: column=ref_code; mask=2/2\nSELECT ref_code FROM orders",
            cx,
        );
        ref_code_result(view, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 0), "AB••••GH");
        assert_eq!(view.masked_columns(0), vec![0]);
        // Copy reads the raw rows through the same export masks.
        let prefs = view.effective_formatting_for_slot(0);
        let rows = crate::result_cell_format::apply_export_masks(
            &view.result_sets[0].columns,
            view.result_sets[0].rows.clone(),
            &prefs,
        );
        assert_eq!(rows[0][0], CellValue::String("AB••••GH".into()));
        let csv = view.export_string(ExportFormat::Csv, cx).expect("csv");
        assert!(csv.contains("AB••••GH"), "{csv}");
        assert!(!csv.contains("ABCDEFGH"), "{csv}");
    });
}

#[gpui::test]
fn qualified_column_override_applies_by_source_table(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        let mut fmt = ResultFormattingPreferences::default();
        let mut mask = crate::preferences::ColumnFormat::default_for_kind(ResultKind::Mask);
        mask.mask_format.keep_prefix = 1;
        mask.mask_format.keep_suffix = 1;
        fmt.column_overrides.insert("orders.ref_code".into(), mask);
        view.formatting = Some(fmt);
        view.set_query_sql("SELECT o.ref_code FROM dbo.orders o", cx);
        ref_code_result(view, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 0), "A••••••H");
        let csv = view.export_string(ExportFormat::Csv, cx).expect("csv");
        assert!(csv.contains("A••••••H"), "{csv}");
    });
    // A different source table leaves the column alone.
    view.update(cx, |view, cx| {
        view.start_query(cx);
        view.set_query_sql("SELECT c.ref_code FROM dbo.customers c", cx);
        ref_code_result(view, cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(grid_cell_text(view, cx, 0, 0), "ABCDEFGH");
    });
}

#[gpui::test]
fn explicit_override_wins_over_auto_mask(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        pin_default_masking(view);
        let mut fmt = ResultFormattingPreferences::default();
        fmt.column_overrides.insert(
            "password_hash".into(),
            crate::preferences::ColumnFormat::default_for_kind(ResultKind::Text),
        );
        view.formatting = Some(fmt);
        view.handle_event(password_result_set(), cx);
        view.handle_event(password_rows(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        let text = grid_cell_text(view, cx, 0, 1);
        assert_eq!(text, "xxxxabcd");
        assert!(view.masked_columns(0).is_empty());
    });
}

#[test]
fn empty_column_indices_finds_all_null_and_ignores_empty_grid() {
    use sqlly_datatable::{CellValue as GridCell, Column, ColumnKind, GridData};
    let empty =
        GridData::new(vec![Column::new("a", ColumnKind::Text, 80.0)], vec![]).expect("empty grid");
    assert!(empty_column_indices(&empty).is_empty());

    let data = GridData::new(
        vec![
            Column::new("empty", ColumnKind::Text, 80.0),
            Column::new("mixed", ColumnKind::Integer, 80.0),
            Column::new("also_empty", ColumnKind::Text, 80.0),
        ],
        vec![
            vec![GridCell::None, GridCell::Integer(1), GridCell::None],
            vec![GridCell::None, GridCell::None, GridCell::None],
        ],
    )
    .expect("rectangular");
    assert_eq!(empty_column_indices(&data), vec![0, 2]);
}

#[gpui::test]
fn run_history_retains_previous_run_on_start_query(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 5;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 12 }, cx);
        view.start_query(cx);
        assert_eq!(view.retained_runs.len(), 1);
        assert_eq!(view.retained_runs[0].sets[0].row_count, 5);
        assert!(view.retained_runs[0].sets[0].rows.is_some());
    });
}

#[gpui::test]
fn run_history_retains_message_only_runs(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 5;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;

        // A script that returns no result sets — only server output — is still
        // a run worth keeping: its messages are its entire content.
        view.set_query_sql("update t set a = 1", cx);
        view.handle_event(
            QueryEvent::Message {
                text: "(3 rows affected)".to_string(),
            },
            cx,
        );
        view.handle_event(QueryEvent::Completed { elapsed_ms: 4 }, cx);
        view.start_query(cx);

        assert_eq!(view.retained_runs.len(), 1, "message-only run is retained");
        assert!(view.retained_runs[0].sets.is_empty());
        assert_eq!(view.retained_runs[0].messages, vec!["(3 rows affected)"]);

        // Restoring it brings the messages back and opens the Messages pane,
        // since there is no grid to show.
        let id = view.retained_runs[0].id;
        view.switch_to_run(Some(id), cx);
        assert_eq!(view.messages, vec!["(3 rows affected)"]);
        assert!(view.show_messages);
    });
}

#[gpui::test]
fn run_history_capture_query_change_dedupes_identical_reruns(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 5;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::QueryChange;

        // First run of query A — always retained (nothing to compare against).
        view.set_query_sql("select * from a", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.start_query(cx);
        assert_eq!(view.retained_runs.len(), 1);

        // Re-run the identical query — same SQL, so QueryChange skips it even
        // though the results differ.
        view.set_query_sql("select * from a", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx);
        view.start_query(cx);
        assert_eq!(
            view.retained_runs.len(),
            1,
            "identical query must not be re-stored"
        );

        // Different query text — retained.
        view.set_query_sql("select * from b", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
        view.start_query(cx);
        assert_eq!(view.retained_runs.len(), 2, "changed query must be stored");
    });
}

#[gpui::test]
fn run_history_disabled_retains_nothing_unless_forced(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 5;
        view.run_history_enabled = false;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;

        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.start_query(cx);
        assert!(
            view.retained_runs.is_empty(),
            "disabled feature keeps no history"
        );

        // An explicit keep-results still overrides the disabled switch.
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx);
        view.keep_results_on_next_run();
        view.start_query(cx);
        assert_eq!(view.retained_runs.len(), 1);
    });
}

#[gpui::test]
fn run_history_is_bounded_and_keeps_pinned_runs(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 2;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..1), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.start_query(cx);
        let first_id = view.retained_runs[0].id;

        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..2), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx);
        view.start_query(cx);
        view.pin_run(first_id, true, cx);
        let second_id = view
            .retained_runs
            .iter()
            .find(|run| run.id != first_id)
            .map(|run| run.id)
            .expect("second run");

        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
        view.start_query(cx);

        let ids: Vec<u64> = view.retained_runs.iter().map(|run| run.id).collect();
        assert!(ids.contains(&first_id), "pinned first run must remain");
        assert!(
            !ids.contains(&second_id),
            "unpinned second run must be evicted"
        );
        assert_eq!(view.retained_runs.len(), 2);
    });
}

#[gpui::test]
fn run_history_zero_keeps_nothing_unless_forced(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 0;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.start_query(cx);
        assert!(view.retained_runs.is_empty());

        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx);
        view.keep_results_on_next_run();
        view.start_query(cx);
        assert_eq!(view.retained_runs.len(), 1);
    });
}

#[gpui::test]
fn run_history_spill_backed_sets_are_count_only(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let spill_file = write_test_spill(10);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut view = ResultsView::new(cx);
        view.run_history_limit = 5;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..10), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.handle_event(spill_manifest_event(&spill_file, 10), cx);
        view.start_query(cx);
        let retained = &view.retained_runs.last().unwrap().sets[0];
        assert!(retained.rows.is_none());
        assert!(retained.unavailable_reason.is_some());
    });
    drop(view);
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

#[gpui::test]
fn switching_to_a_retained_run_installs_read_only_snapshot_and_back(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.run_history_limit = 5;
        view.run_history_enabled = true;
        view.run_history_capture = crate::preferences::RunHistoryCaptureMode::Either;
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 8 }, cx);
        view.start_query(cx);
        let id = view.retained_runs[0].id;

        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 4 }, cx);

        view.switch_to_run(Some(id), cx);
        assert!(view.historical_banner.is_some());
        assert_eq!(view.result_sets[0].rows.len(), 5);

        view.switch_to_run(None, cx);
        assert!(view.historical_banner.is_none());
        assert_eq!(view.result_sets[0].rows.len(), 3);
    });
}

#[test]
fn estimate_set_bytes_grows_with_rows() {
    let column = ResultColumn {
        name: "s".into(),
        data_type: "text".into(),
        nullable: true,
    };
    let small = ResultSet {
        index: 0,
        columns: vec![column.clone()],
        rows: vec![vec![CellValue::String("a".into())]],
        rows_affected: None,
        row_count_hint: 1,
        ..Default::default()
    };
    let large = ResultSet {
        index: 0,
        columns: vec![column],
        rows: vec![
            vec![CellValue::String("a".into())],
            vec![CellValue::String("bb".into())],
        ],
        rows_affected: None,
        row_count_hint: 2,
        ..Default::default()
    };
    assert!(estimate_set_bytes(&large) > estimate_set_bytes(&small));
}

#[gpui::test]
fn truncated_cells_event_populates_sidecar(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "body".into(),
                    data_type: "text".into(),
                    nullable: true,
                }],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::String("x".repeat(8192))]],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: vec![sqlly_services::TruncatedCell {
                    row: 0,
                    column: 0,
                    total_len: 3 * 1024 * 1024,
                    kind: sqlly_services::TruncatedKind::Text,
                }],
            },
            cx,
        );
    });
    view.read_with(cx, |view, _cx| {
        assert!(view.result_sets[0].truncated.contains_key(&(0, 0)));
    });
}

#[gpui::test]
fn truncated_marker_applied_to_grid_text(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "body".into(),
                    data_type: "text".into(),
                    nullable: true,
                }],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::String("x".repeat(8192))]],
            },
            cx,
        );
        view.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: vec![sqlly_services::TruncatedCell {
                    row: 0,
                    column: 0,
                    total_len: 3 * 1024 * 1024,
                    kind: sqlly_services::TruncatedKind::Text,
                }],
            },
            cx,
        );
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.read_with(cx, |view, cx| {
        let grid = view.grid_views[0].as_ref().unwrap();
        let text = grid
            .read(cx)
            .state
            .read(cx)
            .data
            .rows
            .first()
            .and_then(|row| row.first())
            .map(|cell| match cell {
                sqlly_datatable::CellValue::Text(s) => s.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        assert!(
            text.ends_with(" … [3.0 MiB]"),
            "grid cell should show truncation marker, got: {text}"
        );
    });
}

#[test]
fn sync_cell_detail_dedups_unchanged_focus() {
    use crate::cell_detail_model::{FocusedCell, focus_changed};
    let focus = FocusedCell {
        display_row: 1,
        source_row: 1,
        column: 0,
    };
    let last = Some((0, focus));
    assert!(!focus_changed(last, Some((0, focus))));
    assert!(focus_changed(last, Some((1, focus))));
    assert!(focus_changed(
        last,
        Some((
            0,
            FocusedCell {
                display_row: 1,
                source_row: 1,
                column: 2,
            }
        ))
    ));
    assert!(focus_changed(last, None));
}

#[gpui::test]
fn grid_zoom_scales_font_size_and_clamps(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |v, cx| {
        for _ in 0..5 {
            v.adjust_grid_zoom(0.1, cx);
        }
        assert!(
            (v.grid_zoom - 1.5).abs() < 1e-4,
            "grid_zoom after five +0.1 steps: {}",
            v.grid_zoom
        );
        v.adjust_grid_zoom(10.0, cx);
        assert!((v.grid_zoom - 3.0).abs() < 1e-4);
        let base = v.formatting_or_load().density.metrics().font_size;
        assert!((v.zoomed_grid_font_size() - base * 3.0).abs() < 1e-3);
    });
}

#[test]
fn results_font_size_replaces_density_size_and_scales_heights() {
    let normal = GridDensity::Normal.metrics();

    let unset = grid_metrics(GridDensity::Normal, None, crate::editor_zoom::DEFAULT_ZOOM);
    assert_eq!(unset, normal);

    let doubled = normal.font_size * 2.0;
    let custom = grid_metrics(
        GridDensity::Normal,
        Some(doubled),
        crate::editor_zoom::DEFAULT_ZOOM,
    );
    assert_eq!(custom.font_size, doubled);
    assert_eq!(custom.row_height, normal.row_height * 2.0);
    assert_eq!(custom.header_height, normal.header_height * 2.0);

    // Zoom still scales the font on top of the custom size.
    let zoomed = grid_metrics(GridDensity::Normal, Some(doubled), 1.5);
    assert!((zoomed.font_size - doubled * 1.5).abs() < 1e-3);
    assert_eq!(zoomed.row_height, custom.row_height);
}

/// `-- sqlly view: pivot` with a rows-only pivot used to route a re-run
/// through a legacy two-axis pivot builder that rejected it, leaving
/// "Building pivot..." on screen forever. Every run must instead land on
/// the grid's own Pivot tab, which handles single-axis pivots.
#[gpui::test]
fn view_pivot_rerun_opens_the_grid_pivot_tab(cx: &mut gpui::TestAppContext) {
    use sqlly_datatable::GridTab;

    ensure_components_initialized(cx);
    let t = |s: &str| CellValue::String(s.into());
    let sql = "-- sqlly view: pivot\n-- sqlly pivot: row:region; value:amount:sum\nSELECT 1;";
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    for _run in 0..2 {
        view.update(cx, |v, cx| {
            v.start_query(cx);
            v.set_query_sql(sql, cx);
            v.handle_event(
                QueryEvent::ResultSet {
                    index: 0,
                    columns: ["region", "amount"]
                        .iter()
                        .map(|name| ResultColumn {
                            name: (*name).into(),
                            data_type: if *name == "amount" { "float" } else { "text" }.into(),
                            nullable: true,
                        })
                        .collect(),
                },
                cx,
            );
            v.handle_event(
                QueryEvent::Rows {
                    index: 0,
                    rows: vec![
                        vec![t("Europe"), CellValue::F64(10.0)],
                        vec![t("Asia"), CellValue::F64(14.0)],
                    ],
                },
                cx,
            );
            v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        });
        cx.run_until_parked();
        let tab = view.read_with(cx, |v, cx| {
            v.grid_views[0]
                .as_ref()
                .expect("grid view exists")
                .read(cx)
                .active_tab()
        });
        assert_eq!(tab, GridTab::Pivot);
    }
}

/// Full app-integration check for the pivot drill-through with n-tier
/// (multi-level) row grouping and no column fields: build a real
/// `ResultsView` grid, configure a 3-level rows-only pivot, hit-test the
/// leaf value cell exactly as a double-click would, drill, and assert the
/// flat grid lands filtered to the cell's driving rows.
#[gpui::test]
fn pivot_drill_through_ntier_rows_filters_grid(cx: &mut gpui::TestAppContext) {
    use sqlly_datatable::pivot::{PivotHitResult, VisibleRowKind};
    use sqlly_datatable::{GridTab, PivotConfig};

    ensure_components_initialized(cx);
    let t = |s: &str| CellValue::String(s.into());
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: ["region", "product", "color", "amount"]
                    .iter()
                    .map(|name| ResultColumn {
                        name: (*name).into(),
                        data_type: if *name == "amount" { "float" } else { "text" }.into(),
                        nullable: true,
                    })
                    .collect(),
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![
                    vec![t("Europe"), t("Widget"), t("Red"), CellValue::F64(10.0)],
                    vec![t("Europe"), t("Widget"), t("Blue"), CellValue::F64(12.0)],
                    vec![t("Europe"), t("Gadget"), t("Red"), CellValue::F64(13.0)],
                    vec![t("Asia"), t("Widget"), t("Red"), CellValue::F64(14.0)],
                    vec![t("Asia"), t("Gadget"), t("Blue"), CellValue::F64(15.0)],
                ],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    cx.run_until_parked();

    let table = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid view exists").clone()
    });

    // Configure a rows-only n-tier pivot (region > product > color) the way
    // the sidebar drag-drop would, and open the Pivot tab.
    table.update(cx, |table, cx| {
        let pivot = table.pivot_state().expect("pivot enabled").clone();
        pivot.update(cx, |s, cx| {
            s.config = PivotConfig {
                row_fields: vec![0, 1, 2],
                column_fields: vec![],
                value_field: Some(3),
                ..PivotConfig::default()
            };
            s.recompute();
            cx.notify();
        });
        table.set_active_tab(GridTab::Pivot, cx);
    });
    cx.run_until_parked();
    // Force a render pass so the pivot canvas records its bounds.
    cx.simulate_mouse_move(
        gpui::point(px(300.0), px(300.0)),
        Option::<gpui::MouseButton>::None,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    // Locate the Europe > Widget > Red leaf and hit-test the geometry of
    // its value cell — the exact coordinates a double-click would use.
    table.update(cx, |table, cx| {
        let pivot = table.pivot_state().expect("pivot enabled").clone();
        pivot.update(cx, |s, _cx| {
            let leaf_index = s
                .visible_rows()
                .iter()
                .position(|vr| {
                    vr.kind == VisibleRowKind::Leaf
                        && s.row_path_components(vr.key)
                            .iter()
                            .map(|c| c.label.as_str())
                            .collect::<Vec<_>>()
                            == ["Europe", "Widget", "Red"]
                })
                .expect("Europe>Widget>Red leaf visible");
            let x = s.row_header_width + 10.0;
            let y = s.header_height() + (leaf_index as f32 + 0.5) * s.row_height;
            let hit = s.hit_test(gpui::point(px(x), px(y)));
            let PivotHitResult::Cell { row, col } = hit else {
                panic!("value-cell coordinates must hit-test as a cell, got {hit:?}");
            };
            assert_eq!(row, leaf_index, "hit-test row matches the leaf");
            s.request_drill_down(row, col);
        });
        cx.notify();
    });
    // Render pass consumes the queued drill and applies the grid filters.
    cx.simulate_mouse_move(
        gpui::point(px(300.0), px(300.0)),
        Option::<gpui::MouseButton>::None,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    table.read_with(cx, |table, cx| {
        assert_eq!(table.active_tab(), GridTab::Grid, "drill lands on Grid tab");
        assert!(table.drill_filter().is_some(), "drill banner is shown");
        let shown: Vec<usize> = table
            .state
            .read(cx)
            .display_indices
            .iter()
            .copied()
            .collect();
        assert_eq!(
            shown,
            vec![0],
            "grid is filtered to exactly the drilled cell's source row"
        );
    });
}

/// A spill-backed (windowed) slot keeps the Pivot and Chart tabs open: its
/// grid gets a row source over the whole spill file, so the pivot reads all
/// 200,000 rows in the background rather than the ~100 resident ones.
#[gpui::test]
fn spill_backed_slot_pivots_every_row_of_the_spill_file(cx: &mut gpui::TestAppContext) {
    let total = 200_000usize;
    let spill_file = write_test_spill(total);

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();

    let manifest_event = QueryEvent::SpillManifest {
        manifest: sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                row_count: total,
                spill_file: spill_file.clone(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: Some(1),
        },
    };
    view.update(cx, |v, cx| {
        v.handle_event(manifest_event, cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        let table = v.grid_views[0].as_ref().expect("grid exists").read(cx);
        assert!(!table.pivot_locked(), "the Pivot tab stays available");
        assert!(!table.chart_locked(), "the Chart tab stays available");
        let source = table.row_source().expect("the grid reads the spill file");
        assert_eq!(source.row_count(), total);
    });

    // Count rows per name over the whole file: the grand total must be the
    // file's row count, not the resident window's.
    view.update(cx, |v, cx| {
        let table = v.grid_views[0].clone().expect("grid exists");
        table.update(cx, |table, cx| {
            table.enable_pivot(
                sqlly_datatable::PivotConfig {
                    row_fields: vec![1],
                    value_field: Some(0),
                    aggregation: sqlly_datatable::AggregationFn::Count,
                    ..sqlly_datatable::PivotConfig::default()
                },
                cx,
            );
            table.set_active_tab(GridTab::Pivot, cx);
            assert_eq!(table.active_tab(), GridTab::Pivot);
        });
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        let table = v.grid_views[0].as_ref().expect("grid exists").read(cx);
        let pivot = table.pivot_state().expect("pivot").read(cx);
        assert!(!pivot.is_scanning(), "{:?}", pivot.scan_status());
        assert_eq!(pivot.result.source_row_count, total);
        assert_eq!(
            pivot.result.grand_total,
            sqlly_datatable::CellValue::Integer(total as i64)
        );
    });
    let _ = std::fs::remove_file(&spill_file);
    let _ = std::fs::remove_file(format!("{spill_file}.idx"));
}

/// The pivot sidebar's save button must persist a rows-only n-tier pivot
/// (no Columns fields) into the query editor as a `-- sqlly pivot:`
/// directive — previously `ResultPivotSpec::from_config` required both
/// axes, so the save silently did nothing for the everyday
/// "group by several columns" layout.
#[gpui::test]
fn pivot_save_writes_rows_only_directive_into_editor(cx: &mut gpui::TestAppContext) {
    use sqlly_datatable::PivotConfig;

    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: ["region", "product", "amount"]
                    .iter()
                    .map(|name| ResultColumn {
                        name: (*name).into(),
                        data_type: "text".into(),
                        nullable: true,
                    })
                    .collect(),
            },
            cx,
        );
        v.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![
                    CellValue::String("Europe".into()),
                    CellValue::String("Widget".into()),
                    CellValue::String("10".into()),
                ]],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v
    });
    let editor = cx.new_window_entity(|_window, cx| crate::editor::SqlEditor::new(cx));
    cx.update(|window, app| {
        editor.update(app, |e, cx| e.set_text("SELECT 1;", cx));
        let _ = window;
    });
    view.update(cx, |v, cx| {
        v.set_editor(editor.downgrade(), cx);
    });
    cx.run_until_parked();

    // Invoke the registered save handler exactly as the sidebar button does.
    let table = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    let config = PivotConfig {
        row_fields: vec![0, 1],
        column_fields: vec![],
        value_field: Some(2),
        ..PivotConfig::default()
    };
    cx.update(|_window, app| {
        let pivot = table
            .read(app)
            .pivot_state()
            .expect("pivot enabled")
            .clone();
        let handler = pivot
            .read(app)
            .save_config_handler()
            .expect("save handler registered");
        handler(&config, app);
    });
    cx.run_until_parked();

    cx.update(|_window, app| {
        let text = editor.read(app).text().to_string();
        assert!(
            text.contains("-- sqlly pivot: row:region,product; value:amount:sum"),
            "editor gains the rows-only pivot directive, got: {text}"
        );
    });
}

fn id_and_contact_result_set() -> QueryEvent {
    QueryEvent::ResultSet {
        index: 0,
        columns: vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "contact".into(),
                data_type: "varchar(255)".into(),
                nullable: true,
            },
        ],
    }
}

#[gpui::test]
fn suggestion_absent_without_provider(cx: &mut gpui::TestAppContext) {
    // With the decision model disabled (the default), no profile is ever
    // stored, no suggestion appears, and the effective formatting is
    // byte-for-byte what it was before this feature existed.
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.formatting = Some(ResultFormattingPreferences::default());
        view.masking_rules = Some(MaskingRules::default());
        view.handle_event(id_and_contact_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.maybe_spawn_column_profile(cx);
    });
    cx.executor().run_until_parked();
    view.read_with(cx, |view, _cx| {
        assert!(
            view.column_profiles.iter().all(Option::is_none),
            "no provider: no profile may be stored"
        );
        assert!(view.mask_suggestion_columns(0).is_empty());
        assert!(view.masked_columns(0).is_empty());
        let baseline = effective_formatting(
            &view.formatting_or_load(),
            &view.masking_rules_or_load(),
            &view.masking_overrides,
            &view.result_sets[0].columns,
            None,
            &HashSet::new(),
        );
        assert_eq!(
            view.effective_formatting_for_slot(0),
            baseline,
            "formatting must be unchanged while the feature is absent"
        );
    });
}

#[gpui::test]
fn accept_masks_only_suggested_columns(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.formatting = Some(ResultFormattingPreferences::default());
        view.masking_rules = Some(MaskingRules::default());
        view.handle_event(id_and_contact_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        // Install a profile as the background pass would: `id` is an
        // identifier, `contact` holds email addresses.
        view.pad_profile_slots();
        view.column_profiles[0] = Some(vec![
            (crate::column_profile::SemanticType::Identifier, 0.95),
            (crate::column_profile::SemanticType::Email, 0.9),
        ]);
        assert_eq!(view.mask_suggestion_columns(0), vec![1]);
        view.accept_mask_suggestions(cx);
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(
            view.masked_columns(0),
            vec![1],
            "only the suggested personal column is masked"
        );
        let fmt = view.effective_formatting_for_slot(0);
        assert_eq!(
            fmt.column_overrides.get("contact").map(|f| f.kind),
            Some(ResultKind::Mask)
        );
        assert!(
            !fmt.column_overrides.contains_key("id"),
            "the identifier column must keep its formatting"
        );
        assert!(
            view.mask_suggestion_columns(0).is_empty(),
            "nothing is left to suggest once accepted"
        );
    });
}

#[gpui::test]
fn dismissing_hides_the_suggestion_for_the_slot(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.formatting = Some(ResultFormattingPreferences::default());
        view.masking_rules = Some(MaskingRules::default());
        view.handle_event(id_and_contact_result_set(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.pad_profile_slots();
        view.column_profiles[0] = Some(vec![
            (crate::column_profile::SemanticType::Identifier, 0.95),
            (crate::column_profile::SemanticType::Email, 0.9),
        ]);
        assert_eq!(view.mask_suggestion_columns(0), vec![1]);
        view.dismiss_mask_suggestion(cx);
    });
    view.read_with(cx, |view, _cx| {
        assert!(view.mask_suggestion_columns(0).is_empty());
        assert!(
            view.masked_columns(0).is_empty(),
            "dismissing masks nothing"
        );
    });
}

// ---------------------------------------------------------------------------
// Chart suggestion precedence (plan 296)
// ---------------------------------------------------------------------------

fn suggested_chart() -> sqlly_datatable::ChartConfig {
    sqlly_datatable::ChartConfig {
        kind: sqlly_datatable::ChartKind::Bar,
        label_column: Some(1),
        value_columns: vec![2],
        ..Default::default()
    }
}

fn view_with_suggestion() -> ResultsView {
    let mut v = bare_results_view();
    v.result_sets = vec![ResultSet {
        index: 0,
        columns: vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "region".into(),
                data_type: "nvarchar(64)".into(),
                nullable: false,
            },
            ResultColumn {
                name: "total".into(),
                data_type: "decimal(18,2)".into(),
                nullable: false,
            },
        ],
        rows: vec![],
        rows_affected: None,
        row_count_hint: 0,
        truncated: Default::default(),
    }];
    v.pad_chart_suggestion_slots();
    v.chart_suggestions[0] = Some(suggested_chart());
    v
}

#[test]
fn directive_beats_suggestion() {
    let mut v = view_with_suggestion();
    v.view_hints = Some(crate::result_pivot::ResultViewHints {
        default_view: crate::result_pivot::ResultViewKind::Grid,
        pivot: None,
        chart: Some(crate::result_chart::ResultChartSpec::new(
            sqlly_datatable::ChartKind::Pie,
        )),
        diagnostics: vec![],
    });
    let config = v.chart_config_for_slot(0).expect("directive config");
    assert_eq!(config.kind, sqlly_datatable::ChartKind::Pie);
    assert!(
        !v.chart_suggestion_active(0),
        "a directive must hide the suggestion caption"
    );
}

#[test]
fn layout_beats_suggestion() {
    let mut v = view_with_suggestion();
    let saved = sqlly_datatable::ChartConfig {
        kind: sqlly_datatable::ChartKind::Line,
        label_column: Some(0),
        value_columns: vec![2],
        ..Default::default()
    };
    v.chart_configs.insert(0, saved.clone());
    assert_eq!(v.chart_config_for_slot(0), Some(saved));
    assert!(
        !v.chart_suggestion_active(0),
        "a saved layout must hide the suggestion caption"
    );
}

#[gpui::test]
fn revert_restores_auto_chart(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(id_and_contact_result_set(), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.pad_chart_suggestion_slots();
        view.chart_suggestions[0] = Some(suggested_chart());
        assert!(view.chart_suggestion_active(0));
        assert_eq!(view.chart_config_for_slot(0), Some(suggested_chart()));
        view.revert_chart_suggestion(cx);
    });
    view.read_with(cx, |view, _cx| {
        assert_eq!(
            view.chart_config_for_slot(0),
            None,
            "revert must fall back to the chart's own auto-detection"
        );
        assert!(!view.chart_suggestion_active(0));
        assert!(
            view.chart_suggestion_reverted[0],
            "revert pins no-suggestion for the slot"
        );
    });
}

/// The toolbar Export menu used to write the clipboard and say nothing, so a
/// successful copy looked identical to a copy that never happened. The notice
/// is what the toast renders; the toast itself needs a `Root`-backed window,
/// which a gpui test window cannot be.
#[gpui::test]
fn toolbar_export_confirms_the_copy_with_a_row_count(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    view.update(cx, |view, cx| {
        let outcome = view.apply_clipboard_export(ExportFormat::Csv, cx);
        assert_eq!(
            outcome,
            ClipboardExport::Copied {
                rows: 5,
                truncated: false
            },
            "the export reports the rows it actually wrote"
        );
        assert_eq!(
            view.last_export_notice.as_deref(),
            Some("Copied 5 rows as CSV")
        );
    });
    let clipboard = cx.read_from_clipboard();
    assert!(
        clipboard
            .as_ref()
            .and_then(|c| c.text())
            .is_some_and(|t| t.starts_with("id,name")),
        "the rows still reach the clipboard"
    );
}

/// The Export control renders on any tab whose chrome is visible, including one
/// that produced only messages, so it is reachable with no result set behind
/// it. That used to be a dead click: no clipboard write and no report.
#[gpui::test]
fn toolbar_export_reports_when_there_is_nothing_to_export(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        assert_eq!(view.last_export_notice, None, "nothing reported yet");
        let outcome = view.apply_clipboard_export(ExportFormat::Csv, cx);
        assert_eq!(
            outcome,
            ClipboardExport::NothingToExport,
            "no result set means there is nothing to export"
        );
        assert_eq!(
            view.last_export_notice.as_deref(),
            Some("No results to export"),
            "the click must report instead of doing nothing"
        );
    });
}

#[gpui::test]
fn save_chart_to_library_emits_the_live_layout_and_query_sql(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let events: std::rc::Rc<
        std::cell::RefCell<Vec<(crate::result_chart::ResultChartSpec, String)>>,
    > = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::SaveChartToLibraryRequested { spec, sql } = event {
                seen.borrow_mut().push((spec.clone(), sql.clone()));
            }
        })
        .detach();
    });
    // No result set yet: nothing to save, no event.
    view.update(cx, |view, cx| view.request_save_chart_to_library(0, cx));
    assert!(events.borrow().is_empty());

    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id, name FROM t", cx);
        view.handle_event(QueryEvent::Started, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.open_result_chart_tab(0, cx);
    });
    // Tweak the live chart the way the sidebar would (unsaved), then save.
    let grid = view.read_with(cx, |v, _cx| {
        v.grid_views[0].as_ref().expect("grid exists").clone()
    });
    grid.update(cx, |table, cx| {
        let chart = table.chart_state().expect("chart state").clone();
        chart.update(cx, |state, _cx| {
            state.set_config(sqlly_datatable::ChartConfig {
                kind: sqlly_datatable::ChartKind::Donut,
                label_column: Some(1),
                value_columns: vec![0],
                top_n: 7,
                ..sqlly_datatable::ChartConfig::default()
            });
        });
    });
    view.update(cx, |view, cx| {
        assert!(view.chart_tab_in_front(0, cx), "chart tab is in front");
        view.request_save_chart_to_library(0, cx);
    });
    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "one save request expected");
    let (spec, sql) = &recorded[0];
    assert_eq!(sql, "SELECT id, name FROM t");
    assert_eq!(spec.kind, sqlly_datatable::ChartKind::Donut);
    assert_eq!(spec.label_name.as_deref(), Some("name"));
    assert_eq!(spec.value_names, vec!["id".to_string()]);
    assert_eq!(spec.top_n, 7);
    assert_eq!(spec.result_set_index, 0);
}

// ---------------------------------------------------------------------------
// Open in spreadsheet / Copy as SQL / find & replace / staged transforms
// ---------------------------------------------------------------------------

fn replace_test_session() -> sqlly_services::row_editing::RowEditSession {
    use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};
    // Provenance as the service reports it for `SELECT id, name FROM Items`.
    let column = |name: &str, data_type: &str, key: bool, ordinal: usize| RowEditColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        char_max_length: None,
        precision: None,
        scale: None,
        nullable: true,
        is_primary_key: key,
        is_identity: key,
        is_computed: false,
        is_rowversion: false,
        default_definition: None,
        result_ordinal: Some(ordinal),
        enum_values: Vec::new(),
    };
    RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![
            column("id", "int", true, 0),
            column("name", "varchar", false, 1),
        ],
        original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    }
}

#[gpui::test]
fn spreadsheet_export_prefers_xlsx_and_falls_back_to_csv(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let prefs = crate::preferences::ExportPreferences::default();
    let (bytes, format, truncated) = view
        .read_with(cx, |v, cx| v.spreadsheet_export_for_active(&prefs, cx))
        .expect("fixture has a result set");
    assert_eq!(format, crate::results::ExportFormat::Xlsx);
    assert!(!truncated);
    assert!(
        bytes.starts_with(&0x0403_4b50u32.to_le_bytes()),
        "xlsx temp payload is the real workbook (zip)"
    );
    let csv_prefs = crate::preferences::ExportPreferences {
        xlsx: false,
        ..Default::default()
    };
    let (bytes, format, _) = view
        .read_with(cx, |v, cx| v.spreadsheet_export_for_active(&csv_prefs, cx))
        .expect("fixture has a result set");
    assert_eq!(format, crate::results::ExportFormat::Csv);
    let text = String::from_utf8(bytes).expect("csv is utf-8");
    assert!(
        text.replace("\r\n", "\n").starts_with("\u{FEFF}id,name\n"),
        "CSV opens in Excel as UTF-8 (BOM first): {text}"
    );
}

#[gpui::test]
fn spreadsheet_export_is_none_without_a_result_set(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let prefs = crate::preferences::ExportPreferences::default();
    assert!(
        view.read_with(cx, |v, cx| v.spreadsheet_export_for_active(&prefs, cx))
            .is_none()
    );
}

#[gpui::test]
fn copy_preview_sql_puts_the_script_on_the_clipboard(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    // Nothing staged: copies nothing and reports empty.
    let copied = view.update(cx, |view, cx| view.copy_preview_sql(cx));
    assert!(copied.is_empty());
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]),
                original_row: vec![CellValue::I64(3), CellValue::String("Bob".into())],
                changes: [(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("Bobby".into())),
                )]
                .into_iter()
                .collect(),
            },
            cx,
        );
    });
    // Copied immediately — before the debounced preview refresh has run —
    // the script is rendered from the staged set, not the stale preview.
    let copied = view.update(cx, |view, cx| view.copy_preview_sql(cx));
    assert!(copied.contains("UPDATE"), "{copied}");
    assert!(copied.contains("Bobby"), "{copied}");
    let expected = view.read_with(cx, |v, _| v.staged_change_script());
    assert_eq!(copied, expected);
    let clipboard_text = cx
        .read_from_clipboard()
        .and_then(|c| c.text())
        .unwrap_or_default();
    assert_eq!(clipboard_text, copied);
}

/// Opens a replace session on slot 0 by staging a manual edit on row `id`
/// (the state a real replace runs in: session open, marker column shown).
fn stage_manual_name_edit(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
    id: i64,
    original: &str,
    edit: crate::pending_changes::CellEdit,
) {
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(id)]),
                original_row: vec![CellValue::I64(id), CellValue::String(original.into())],
                changes: [("name".to_string(), edit)].into_iter().collect(),
            },
            cx,
        );
        view.query_sql = Some("SELECT id, name FROM dbo.Items".to_string());
    });
}

#[gpui::test]
fn grid_find_replace_uses_the_needle_the_grid_searched(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    // Row 3 now reads "Bob Bobby"; the query "Bob " (trailing space) matches
    // it. Replacing the trimmed "Bob" would also rewrite "Bobby".
    stage_manual_name_edit(
        &view,
        cx,
        3,
        "Bob",
        crate::pending_changes::CellEdit::Value(CellValue::String("Bob Bobby".into())),
    );
    view.update(cx, |view, cx| {
        view.grid_find.query = "Bob ".to_string();
        view.grid_find.replace = "X".to_string();
        view.apply_grid_find(cx);
    });
    view.read_with(cx, |v, cx| assert_eq!(v.grid_find_match_count(cx), 1));
    view.update(cx, |view, cx| view.grid_find_replace(true, cx));
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]);
        assert_eq!(
            v.pending.current().dirty[&key]["name"],
            crate::pending_changes::CellEdit::Value(CellValue::String("XBobby".into())),
        );
    });
}

#[gpui::test]
fn grid_find_replace_keeps_a_staged_expression(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let expression = crate::pending_changes::CellEdit::Expression(
        sqlly_services::row_editing::ColumnExpression::ServerUuid,
    );
    stage_manual_name_edit(&view, cx, 1, "Alice", expression.clone());
    // The source row still says "Alice", but the cell's staged value is an
    // expression: replace must not overwrite it with rewritten text.
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_replace(
            0,
            session,
            vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]],
            vec![(0, 1)],
            "Alice".to_string(),
            "X".to_string(),
            cx,
        );
    });
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]);
        assert_eq!(v.pending.current().dirty[&key]["name"], expression);
    });
}

#[gpui::test]
fn grid_find_replace_skips_rows_staged_for_deletion(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = replace_test_session();
    let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]);
    view.update(cx, |view, cx| {
        view.pending_slot = Some(0);
        view.pending.stage_delete(
            session.clone(),
            key.clone(),
            vec![CellValue::I64(3), CellValue::String("Bob".into())],
        );
        view.apply_staged_replace(
            0,
            session,
            vec![vec![CellValue::I64(3), CellValue::String("Bob".into())]],
            vec![(0, 1)],
            "Bob".to_string(),
            "Robert".to_string(),
            cx,
        );
    });
    view.read_with(cx, |v, _cx| {
        assert!(
            !v.pending.current().dirty.contains_key(&key),
            "an UPDATE after the row's DELETE would fail the whole save"
        );
        assert!(v.pending.current().deleted.contains(&key));
        assert!(
            v.messages.iter().any(|m| m.contains("staged for deletion")),
            "{:?}",
            v.messages
        );
    });
}

#[gpui::test]
fn grid_find_replace_never_writes_an_aliased_expression_column(cx: &mut gpui::TestAppContext) {
    // SELECT id, left(name, 3) AS name — the result column shares the table
    // column's name but provenance leaves `name` unprojected.
    let (view, cx) = make_results_view_with_data(cx);
    let mut session = replace_test_session();
    session.columns[1].result_ordinal = None;
    view.update(cx, |view, cx| {
        view.apply_staged_replace(
            0,
            session,
            vec![vec![CellValue::I64(1), CellValue::String("Ali".into())]],
            vec![(0, 1)],
            "Ali".to_string(),
            "X".to_string(),
            cx,
        );
    });
    view.read_with(cx, |v, _cx| {
        assert!(v.pending.is_empty(), "nothing may be staged into `name`");
    });
}

#[test]
fn replace_all_coverage_note_reports_unsearched_rows() {
    assert_eq!(replace_all_coverage_note(None, None), None);
    // A window holding the whole set covered everything.
    assert_eq!(replace_all_coverage_note(Some((0, 50, 50)), None), None);
    let note = replace_all_coverage_note(Some((3_000, 100, 5_000)), None).unwrap();
    assert!(
        note.contains("only the 100 loaded rows (3001–3100 of 5000)"),
        "{note}"
    );
    let capped = replace_all_coverage_note(None, Some(100_000)).unwrap();
    assert!(capped.contains("100000-match limit"), "{capped}");
}

#[test]
fn backing_row_shifts_only_windowed_rows() {
    assert_eq!(backing_row_for_grid_row(None, 7), 7);
    assert_eq!(backing_row_for_grid_row(Some(3_000), 5), 3_005);
    assert_eq!(backing_row_for_grid_row(Some(usize::MAX), 5), usize::MAX);
}

#[test]
fn paste_fills_only_a_contiguous_range_of_data_columns() {
    // Rows 2..=6, grid columns 1..=3 with a one-wide row-marker column.
    assert_eq!(
        paste_fill_range(&Selection::CellRange(2, 1, 6, 3), 1),
        Some((5, 3))
    );
    // A range that starts on the marker counts only the data columns.
    assert_eq!(
        paste_fill_range(&Selection::CellRange(0, 0, 1, 2), 1),
        Some((2, 2))
    );
    assert_eq!(paste_fill_range(&Selection::CellRange(0, 0, 3, 0), 1), None);
    assert_eq!(paste_fill_range(&Selection::Cell(4, 2), 1), None);
    assert_eq!(
        paste_fill_range(&Selection::Cells(vec![(0, 1), (3, 1)]), 1),
        None
    );
}

/// Regression: a spill-backed grid windowed away from the top reports
/// resident-relative rows. Paste-overwrite targets (`display_row_source`)
/// and find & replace must page the row on screen, not the row at the same
/// resident index from the top of the spill.
#[gpui::test]
fn spill_backed_replace_and_row_lookup_use_the_window_offset(cx: &mut gpui::TestAppContext) {
    let total = 5_000usize;
    let spill_file = write_test_spill(total);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    // Open the edit session (adds the pending marker column) with an edit
    // on row 1, as a real replace would find it.
    stage_manual_name_edit(
        &view,
        cx,
        1,
        "row1",
        crate::pending_changes::CellEdit::Value(CellValue::String("first".into())),
    );
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        let grid_state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        let marker = view.marker_offset(0);
        assert_eq!(marker, 1, "pending marker column is shown");
        // Page the resident window to rows 3000..3100 (marker cell first).
        let rows: Vec<Vec<sqlly_datatable::CellValue>> = (3_000..3_100)
            .map(|i| {
                vec![
                    sqlly_datatable::CellValue::None,
                    sqlly_datatable::CellValue::Integer(i),
                    sqlly_datatable::CellValue::Text(format!("row{i}")),
                ]
            })
            .collect();
        grid_state.update(cx, |s, _| {
            s.set_row_window(total, 3_000, rows).expect("window");
        });
        assert_eq!(
            view.display_row_source(0, 3_005, cx),
            Some(vec![
                CellValue::I64(3_005),
                CellValue::String("row3005".into())
            ]),
            "display row 3005 is spill row 3005, not resident row 5"
        );
        view.grid_find.query = "row3005".to_string();
        view.grid_find.replace = "X".to_string();
        view.apply_grid_find(cx);
        assert_eq!(view.grid_find_match_count(cx), 1);
        view.grid_find_replace(true, cx);
    });
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(3_005)]);
        assert_eq!(
            v.pending
                .current()
                .dirty
                .get(&key)
                .map(|edits| &edits["name"]),
            Some(&crate::pending_changes::CellEdit::Value(CellValue::String(
                "X".into()
            ))),
            "messages: {:?}",
            v.messages
        );
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("Replace All covered only the 100 loaded rows")),
            "{:?}",
            v.messages
        );
    });
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

/// View Full Value and the Cell Details dialog start from the grid-resident
/// row. On a spill-backed grid scrolled to rows 3000..3100, resident row 5
/// is spill row 3005: the truncation entry and the hydrated row must be the
/// absolute row's, not row 5's from the top of the spill.
#[gpui::test]
fn spill_backed_hydration_uses_the_window_offset(cx: &mut gpui::TestAppContext) {
    let total = 5_000usize;
    let spill_file = write_test_spill(total);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        v.handle_event(spill_manifest_event(&spill_file, total), cx);
        v.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: vec![sqlly_services::TruncatedCell {
                    row: 3_005,
                    column: 1,
                    total_len: 3 * 1024 * 1024,
                    kind: sqlly_services::TruncatedKind::Text,
                }],
            },
            cx,
        );
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        v.query_sql = Some("SELECT id, name FROM t".into());
    });
    cx.run_until_parked();
    let hydrated: std::rc::Rc<std::cell::RefCell<Vec<Vec<CellValue>>>> = Default::default();
    let sink = hydrated.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::HydrateCellRequested { row, .. } = event {
                sink.borrow_mut().push(row.clone());
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        let grid_state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        let rows: Vec<Vec<sqlly_datatable::CellValue>> = (3_000..3_100)
            .map(|i| {
                vec![
                    sqlly_datatable::CellValue::Integer(i),
                    sqlly_datatable::CellValue::Text(format!("row{i}")),
                ]
            })
            .collect();
        grid_state.update(cx, |s, _| {
            s.set_row_window(total, 3_000, rows).expect("window");
        });
        let offset = grid_state.read(cx).window.map(|w| w.offset);
        assert_eq!(offset, Some(3_000));
        let row = view.truncation_row_for_grid(0, 5, offset);
        assert_eq!(row, 3_005, "resident row 5 is spill row 3005");
        assert!(view.truncated_cell(0, row, 1).is_some());
        view.request_cell_hydration(0, row, 1, crate::cell_hydration::HydratePurpose::Viewer, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        hydrated.borrow().as_slice(),
        &[vec![
            CellValue::I64(3_005),
            CellValue::String("row3005".into())
        ]],
        "hydration pages the absolute spill row"
    );
    std::fs::remove_file(&spill_file).ok();
    std::fs::remove_file(format!("{spill_file}.idx")).ok();
}

#[gpui::test]
fn grid_find_replace_composes_with_an_earlier_staged_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session.clone(),
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]),
                original_row: vec![CellValue::I64(3), CellValue::String("Bob".into())],
                changes: [(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("Bobby".into())),
                )]
                .into_iter()
                .collect(),
            },
            cx,
        );
        // The source row still reads "Bob"; the staged value "Bobby" is what
        // the replace must rewrite.
        view.apply_staged_replace(
            0,
            session,
            vec![vec![CellValue::I64(3), CellValue::String("Bob".into())]],
            vec![(0, 1)],
            "bby".to_string(),
            "b!".to_string(),
            cx,
        );
    });
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]);
        assert_eq!(
            v.pending.current().dirty[&key]["name"],
            crate::pending_changes::CellEdit::Value(CellValue::String("Bob!".into()))
        );
    });
}

#[gpui::test]
fn grid_find_replace_stages_matches_through_the_open_session(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    // Stage an unrelated manual edit first — this opens the session and puts
    // the marker column on the grid, the state a real replace runs in.
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]),
                original_row: vec![CellValue::I64(3), CellValue::String("Bob".into())],
                changes: [(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("Bobby".into())),
                )]
                .into_iter()
                .collect(),
            },
            cx,
        );
        view.query_sql = Some("SELECT id, name FROM dbo.Items".to_string());
        view.grid_find.query = "Alice".to_string();
        view.grid_find.replace = "Alicia".to_string();
        view.apply_grid_find(cx);
    });
    view.read_with(cx, |v, cx| {
        assert_eq!(v.grid_find_match_count(cx), 1, "one match for Alice");
    });
    view.update(cx, |view, cx| view.grid_find_replace(true, cx));
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]);
        assert_eq!(
            v.pending.current().dirty[&key]["name"],
            crate::pending_changes::CellEdit::Value(CellValue::String("Alicia".into())),
            "the match became a staged edit"
        );
        let other = crate::pending_changes::RowKey::new(vec![CellValue::I64(3)]);
        assert_eq!(
            v.pending.current().dirty[&other]["name"],
            crate::pending_changes::CellEdit::Value(CellValue::String("Bobby".into())),
            "the manual staged edit is untouched"
        );
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("Staged 1 replacement")),
            "report message expected, got {:?}",
            v.messages
        );
    });
}

#[gpui::test]
fn grid_find_replace_skips_matches_that_no_longer_contain_the_needle(
    cx: &mut gpui::TestAppContext,
) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        // A match whose current value holds no occurrence produces no edit.
        view.apply_staged_replace(
            0,
            session,
            vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]],
            vec![(0, 1)],
            "zzz".to_string(),
            "x".to_string(),
            cx,
        );
    });
    view.read_with(cx, |v, _cx| {
        assert!(v.pending.is_empty(), "nothing staged");
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("No replaceable matches")),
            "skip message expected, got {:?}",
            v.messages
        );
    });
}

#[gpui::test]
fn grid_find_replace_without_a_connection_reports(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.query_sql = Some("SELECT id, name FROM dbo.Items".to_string());
        view.ensure_grid_for_active_tab(cx);
        view.grid_find.query = "Alice".to_string();
        view.apply_grid_find(cx);
        view.grid_find_replace(true, cx);
    });
    view.read_with(cx, |v, _cx| {
        assert!(v.pending.is_empty());
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("Connect to a database")),
            "no-session path should explain itself, got {:?}",
            v.messages
        );
    });
}

#[gpui::test]
fn transform_staged_values_rewrites_staged_strings(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]),
                original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
                changes: [(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("alice".into())),
                )]
                .into_iter()
                .collect(),
            },
            cx,
        );
        view.transform_staged_values(crate::cell_transform::CellTransform::Upper, cx);
    });
    view.read_with(cx, |v, _cx| {
        let key = crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]);
        assert_eq!(
            v.pending.current().dirty[&key]["name"],
            crate::pending_changes::CellEdit::Value(CellValue::String("ALICE".into()))
        );
        assert!(
            v.messages.iter().any(|m| m.contains("1 staged value")),
            "count message expected, got {:?}",
            v.messages
        );
    });
}

#[gpui::test]
fn transform_staged_values_reports_when_nothing_changes(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let session = replace_test_session();
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key: crate::pending_changes::RowKey::new(vec![CellValue::I64(1)]),
                original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
                changes: [(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("ALICE".into())),
                )]
                .into_iter()
                .collect(),
            },
            cx,
        );
        view.transform_staged_values(crate::cell_transform::CellTransform::Upper, cx);
    });
    view.read_with(cx, |v, _cx| {
        assert!(
            v.messages
                .iter()
                .any(|m| m.contains("no staged plain-text value changed")),
            "no-op message expected, got {:?}",
            v.messages
        );
    });
}

/// A grid filtered down to zero visible rows must yield zero rows for
/// export/copy — not fall back to the whole result set (which would put
/// every row on the clipboard or in the file while the grid shows none).
#[gpui::test]
fn rows_from_grid_state_is_empty_when_a_filter_hides_every_row(cx: &mut gpui::TestAppContext) {
    use sqlly_datatable::{
        CellValue as GridCell, Column as GridColumn, ColumnKind, GridData, SqllyDataTable,
    };
    let data = GridData::new(
        vec![GridColumn {
            name: "v".into(),
            kind: ColumnKind::Text,
            width: 80.0,
        }],
        vec![
            vec![GridCell::Text("a".into())],
            vec![GridCell::Text("b".into())],
        ],
    )
    .unwrap();
    let table = cx.update(|cx| {
        SqllyDataTable::builder(data)
            .theme(crate::grid_bridge::build_grid_theme())
            .build(cx)
    });
    let rs_rows = vec![
        vec![CellValue::String("a".into())],
        vec![CellValue::String("b".into())],
    ];
    cx.update(|cx| {
        let state = table.state.read(cx);
        assert_eq!(super::rows_from_grid_state(state, &rs_rows, |i| i).len(), 2);
    });
    cx.update(|cx| {
        table.state.update(cx, |state, _| {
            state.display_indices = std::sync::Arc::new(Vec::new());
        });
        let state = table.state.read(cx);
        assert!(super::rows_from_grid_state(state, &rs_rows, |i| i).is_empty());
    });
    cx.update(|cx| {
        table.state.update(cx, |state, _| {
            state.display_indices = std::sync::Arc::new(vec![1]);
        });
        let state = table.state.read(cx);
        assert_eq!(
            super::rows_from_grid_state(state, &rs_rows, |i| i),
            vec![vec![CellValue::String("b".into())]]
        );
    });
}

#[gpui::test]
fn columns_auto_size_to_values_and_leave_dragged_columns_alone(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let width = |view: &gpui::Entity<ResultsView>, cx: &mut gpui::VisualTestContext, col: usize| {
        view.read_with(cx, |view, cx| {
            view.grid_views[0]
                .as_ref()
                .expect("grid exists")
                .read(cx)
                .state
                .read(cx)
                .data
                .columns[col]
                .width
        })
    };
    let cap = crate::preferences::DEFAULT_AUTO_SIZE_MAX_CHARS;
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
    });
    let name_col = view.read_with(cx, |view, _| view.result_sets[0].columns[1].name.clone());
    assert_eq!(
        width(&view, cx, 1),
        crate::grid_bridge::auto_column_width(&name_col, 4, cap)
    );

    // A longer value arriving later widens the column, up to the cap.
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::I64(9), CellValue::String("x".repeat(80))]],
            },
            cx,
        );
    });
    assert_eq!(
        width(&view, cx, 1),
        crate::grid_bridge::auto_column_width(&name_col, 80, cap)
    );

    // Once the user drags a column, later rows never resize it.
    view.update(cx, |view, cx| {
        let state = view.grid_views[0].as_ref().unwrap().read(cx).state.clone();
        state.update(cx, |s, _| s.data.columns[1].width = 77.0);
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::I64(10), CellValue::String("y".repeat(5))]],
            },
            cx,
        );
    });
    assert_eq!(width(&view, cx, 1), 77.0);
}

/// Auto widths are measurements, not a layout choice: they must never be
/// saved as the query's remembered column layout, or the next run restores
/// them, mistakes them for widths the user picked, and stops auto-sizing —
/// the "works once per query" bug. A dragged width is still remembered.
#[gpui::test]
fn auto_sizing_survives_a_remembered_layout_and_keeps_dragged_widths(
    cx: &mut gpui::TestAppContext,
) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let width = |view: &gpui::Entity<ResultsView>, cx: &mut gpui::VisualTestContext, col: usize| {
        view.read_with(cx, |view, cx| {
            view.grid_views[0]
                .as_ref()
                .expect("grid exists")
                .read(cx)
                .state
                .read(cx)
                .data
                .columns[col]
                .width
        })
    };
    let cap = crate::preferences::DEFAULT_AUTO_SIZE_MAX_CHARS;
    let scope = crate::column_layout::LayoutScope {
        server: "h".into(),
        database: "d".into(),
        default_schema: "dbo".into(),
        ..Default::default()
    };
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(scope.clone()));
        view.set_layout_store_for_test(crate::column_layout::ColumnLayoutStore::default());
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    let name_col = view.read_with(cx, |view, _| view.result_sets[0].columns[1].name.clone());
    let first_auto = crate::grid_bridge::auto_column_width(&name_col, 4, cap);
    assert_eq!(width(&view, cx, 1), first_auto);
    // Sizing the columns is not a layout change: nothing was scheduled.
    view.read_with(cx, |view, _| {
        assert_eq!(view.layout_save_generation.first().copied().unwrap_or(0), 0);
    });

    // The user drags the id column; that change is saved — with the
    // auto-sized name column recorded at its default, not its measured width.
    let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
    view.update(cx, |view, cx| {
        let state = view.grid_views[0].as_ref().unwrap().read(cx).state.clone();
        state.update(cx, |s, cx| {
            s.data.columns[0].width = 77.0;
            cx.notify();
        });
        view.note_possible_layout_change(0, cx);
        let generation = view.layout_save_generation[0];
        assert!(generation > 0, "a dragged width schedules a save");
        view.flush_layout_save(0, generation, cx);
        let sig = crate::column_layout::signature_for(&view.result_sets[0].columns);
        let saved = view
            .layout_store()
            .layout_for(&key, &sig)
            .expect("the drag was remembered")
            .clone();
        assert_eq!(saved.widths[0], 77.0);
        assert_eq!(
            saved.widths[1],
            crate::grid_bridge::column_width_for_name(&name_col),
            "the auto-sized column is remembered at its default, not its measurement"
        );
    });

    // Second run of the same query with longer values: the remembered layout
    // is applied, the dragged column stays, and the other column is sized to
    // the new values.
    view.update(cx, |view, cx| {
        view.clear(cx);
        view.set_query_sql("select * from t", cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::I64(1), CellValue::String("x".repeat(20))]],
            },
            cx,
        );
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    assert_eq!(width(&view, cx, 0), 77.0, "the dragged width wins");
    let second_auto = crate::grid_bridge::auto_column_width(&name_col, 20, cap);
    assert_ne!(second_auto, first_auto);
    assert_eq!(width(&view, cx, 1), second_auto, "auto-size still runs");
    // And a later row keeps resizing it on this run too.
    view.update(cx, |view, cx| {
        view.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::I64(2), CellValue::String("y".repeat(28))]],
            },
            cx,
        );
    });
    assert_eq!(
        width(&view, cx, 1),
        crate::grid_bridge::auto_column_width(&name_col, 28, cap)
    );
    assert_eq!(width(&view, cx, 0), 77.0);
}

/// The WHERE action ids a menu lists.
/// Every item of a context menu with its submenus opened in place, so tests
/// can look for an action wherever the menu files it.
fn flat_menu(
    items: Vec<sqlly_datatable::ContextMenuItem>,
) -> Vec<sqlly_datatable::ContextMenuItem> {
    let mut out = Vec::new();
    for item in items {
        match item {
            sqlly_datatable::ContextMenuItem::Submenu { items, .. } => out.extend(flat_menu(items)),
            other => out.push(other),
        }
    }
    out
}

fn where_menu_ids(items: &[sqlly_datatable::ContextMenuItem]) -> Vec<String> {
    flat_menu(items.to_vec())
        .iter()
        .filter_map(|item| match item {
            sqlly_datatable::ContextMenuItem::Action { id, .. } if id.starts_with("where_") => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

/// A right-click on grid column 1 of a two-column row.
fn two_column_cell_request(
    first: sqlly_datatable::CellValue,
    second: sqlly_datatable::CellValue,
) -> sqlly_datatable::ContextMenuRequest {
    sqlly_datatable::ContextMenuRequest::for_test(
        sqlly_datatable::ContextMenuTarget::Cell {
            display_row_index: 0,
            source_row_index: 0,
            column_index: 1,
        },
        None,
        vec![vec![first, second]],
        vec![
            sqlly_datatable::ColumnContext {
                index: 0,
                name: "id".into(),
                kind: sqlly_datatable::ColumnKind::Integer,
            },
            sqlly_datatable::ColumnContext {
                index: 1,
                name: "notes".into(),
                kind: sqlly_datatable::ColumnKind::Text,
            },
        ],
    )
}

fn two_columns(second_type: &str) -> Vec<ResultColumn> {
    vec![
        ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        },
        ResultColumn {
            name: "notes".into(),
            data_type: second_type.into(),
            nullable: true,
        },
    ]
}

/// A SQLite TEXT column may hold a BLOB the grid shows as its text: the
/// menu offers the text patterns, and the click, which reads the raw bytes,
/// builds them (checked against the builder in the menu's own tests).
#[gpui::test]
fn sqlite_text_column_menu_offers_what_its_builder_builds(cx: &mut gpui::TestAppContext) {
    let (view, _cx) = make_results_view_with_data(cx);
    let columns = two_columns("TEXT");
    let provider = ResultGridMenuProvider::new(view.downgrade(), None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default())
        .with_column_types(columns.iter().map(|c| c.data_type.clone()).collect())
        .with_columns(columns, 0)
        .with_source_db(Some(sqlly_lang::sqlmodel::DatabaseType::Sqlite));
    let request = two_column_cell_request(
        sqlly_datatable::CellValue::Integer(7),
        sqlly_datatable::CellValue::Text("hello".into()),
    );
    let ids = where_menu_ids(&provider.menu_items(&request));
    for id in [
        crate::result_grid_menu::action_id::WHERE_EQUALS,
        crate::result_grid_menu::action_id::WHERE_LIKE,
        crate::result_grid_menu::action_id::WHERE_STARTS_WITH,
        crate::result_grid_menu::action_id::WHERE_MATCH_ROW,
    ] {
        assert!(ids.iter().any(|i| i == id), "{id} missing from {ids:?}");
    }
}

/// Every cell the host cut to a preview carries the marker in its grid
/// text, whatever its variant, so the menu (which reads the text) never
/// offers a value filter or Match Row on it, the same as the click.
#[gpui::test]
fn every_cut_cell_is_marked_and_offers_no_value_filter(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let mut rows = vec![vec![
        sqlly_datatable::CellValue::Integer(1),
        sqlly_datatable::CellValue::Integer(42),
    ]];
    view.update(cx, |v, _cx| {
        v.result_sets[0].truncated.insert(
            (0, 1),
            TruncatedCell {
                row: 0,
                column: 1,
                total_len: 3 * 1024 * 1024,
                kind: sqlly_services::TruncatedKind::Text,
            },
        );
        v.apply_truncation_markers(0, 0, &mut rows);
    });
    let shown = match &rows[0][1] {
        sqlly_datatable::CellValue::Text(t) => t.clone(),
        other => panic!("a cut cell is shown as marked text, got {other:?}"),
    };
    assert!(shown.starts_with("42"), "{shown}");
    assert!(
        crate::result_grid_menu::looks_truncated_marker(&shown),
        "{shown}"
    );

    let columns = two_columns("varchar");
    let provider = ResultGridMenuProvider::new(view.downgrade(), None, 0)
        .with_export_prefs(crate::preferences::ExportPreferences::default())
        .with_column_types(columns.iter().map(|c| c.data_type.clone()).collect())
        .with_columns(columns, 0)
        .with_source_db(Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer));
    let request =
        two_column_cell_request(sqlly_datatable::CellValue::Integer(1), rows[0][1].clone());
    let ids = where_menu_ids(&provider.menu_items(&request));
    assert!(
        !ids.iter()
            .any(|i| i == crate::result_grid_menu::action_id::WHERE_EQUALS
                || i == crate::result_grid_menu::action_id::WHERE_MATCH_ROW),
        "{ids:?}"
    );
    assert!(
        ids.iter()
            .any(|i| i == crate::result_grid_menu::action_id::WHERE_IS_NULL),
        "{ids:?}"
    );
}

/// The lock writes what is on screen; running the locked query brings the
/// grid back exactly — order, frozen columns, widths (font-relative), sort,
/// filters, and conditional rules.
#[gpui::test]
fn locking_then_rerunning_reproduces_the_grid(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let sql = "-- sqlly conditional: column=name; when=is a; bold\nSELECT id, name FROM people";
    let rows = [(1, "b"), (2, "a"), (3, "c"), (4, "a")];
    let snapshot = view.update(cx, |view, cx| {
        view.set_query_sql(sql, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&rows), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        state.update(cx, |s, _| {
            s.data.columns[1].width = 200.0;
            s.set_column_order(&[1, 0]);
            s.set_frozen_columns(1);
            s.filters[1] = sqlly_datatable::ColumnFilter {
                predicate: sqlly_datatable::FilterPredicate::Text {
                    op: sqlly_datatable::TextOp::Is,
                    operand: "a".into(),
                },
                values: None,
            };
            s.sort = Some((0, sqlly_datatable::SortDirection::Descending));
            s.recompute();
        });
        view.lock_snapshot(0, cx)
    });
    let text = snapshot.lines.join("\n");
    assert!(
        text.contains("-- sqlly grid: order=name, id; freeze=1; sort=id desc; filter=name is a"),
        "{text}"
    );
    assert!(
        text.contains("-- sqlly format: column=name; width="),
        "{text}"
    );
    assert!(
        text.contains("-- sqlly conditional: column=name; when=is a; bold"),
        "{text}"
    );

    let locked = crate::result_lock::apply_lock(sql, (0, sql.len()), &snapshot.lines);
    assert!(locked.text.starts_with(crate::sqlly_directive::LOCK_OPENER));
    assert!(crate::sqlly_directive::directive_diagnostics(&locked.text).is_empty());

    view.update(cx, |view, cx| {
        view.start_query(cx);
        view.set_query_sql(&locked.text, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&rows), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(view.query_is_locked());
        // Locking again changes nothing: the block already says it all.
        let again = view.lock_snapshot(0, cx);
        let relocked = crate::result_lock::apply_lock(&locked.text, locked.span, &again.lines);
        assert!(!relocked.changed, "{}", relocked.text);
    });
    view.read_with(cx, |view, cx| {
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .read(cx);
        assert_eq!(state.column_order(), &[1, 0]);
        assert_eq!(state.frozen_columns(), 1);
        assert!(
            (state.data.columns[1].width - 200.0).abs() < 3.0,
            "{}",
            state.data.columns[1].width
        );
        assert!(state.filters[1].is_active());
        assert_eq!(
            state.sort,
            Some((0, sqlly_datatable::SortDirection::Descending))
        );
        assert_eq!(visible_ids(view, cx), vec![4, 2]);
    });
}

/// `-- sqlly column:` adds Excel-formula columns as rows stream in, reading
/// earlier rows across batches, and the lock writes them back.
#[gpui::test]
fn computed_columns_extend_streamed_rows(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let sql = "-- sqlly column: Label = UPPER(!name) & \"-\" & !id; after=id\n\
               -- sqlly column: Running = !id + N(!Running[1])\nSELECT id, name FROM people";
    view.update(cx, |view, cx| {
        view.set_query_sql(sql, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(1..3), cx);
        view.handle_event(rows_batch(3..5), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let rs = &view.result_sets[0];
        let names: Vec<&str> = rs.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "Label", "name", "Running"]);
        assert_eq!(rs.rows[0][1], CellValue::String("ROW1-1".into()));
        assert_eq!(
            rs.rows[3][3],
            CellValue::F64(10.0),
            "1+2+3+4 across two batches"
        );
        let lines = view.lock_snapshot(0, cx).lines;
        assert!(
            lines.contains(
                &"-- sqlly column: Label = UPPER(!name) & \"-\" & !id; after=id".to_string()
            ),
            "{lines:?}"
        );
    });
    view.read_with(cx, |view, cx| {
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .read(cx);
        assert_eq!(state.data.columns.len(), 4);
    });
}

/// The Excel export keeps the grid: display order, widths, frozen columns,
/// computed columns as live formulas with their values cached, `@variables`
/// as defined names, and a pivot sheet when a pivot is configured.
#[gpui::test]
fn excel_export_writes_formulas_layout_and_pivot(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let sql = "-- sqlly set bonus = 10\n\
               -- sqlly name 1: People\n\
               -- sqlly column: Score = !id * 2 + @bonus; after=id\n\
               -- sqlly column: Delta = !id - !id[1]\n\
               -- sqlly format: column=Score; number=1\n\
               -- sqlly conditional: column=Score; when=> 15; bold\n\
               -- sqlly pivot: row:name; value:id:sum\n\
               SELECT id, name FROM people";
    let bytes = view.update(cx, |view, cx| {
        view.set_formula_variables(crate::computed_columns::formula_variables(sql, &[]));
        view.set_query_sql(sql, cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(1..4), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        state.update(cx, |s, _| s.set_frozen_columns(1));
        view.prepare_excel_export(0, cx);
        let rs = view.result_sets[0].clone();
        view.excel_workbook(0, &rs.columns, &rs.rows, cx)
    });
    if let Ok(path) = std::env::var("SQLLY_XLSX_SAMPLE") {
        std::fs::write(path, &bytes).expect("sample written");
    }
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("<sheet name=\"People\""),
        "sheet named after the result"
    );
    assert!(text.contains("<sheet name=\"People pivot\""), "pivot sheet");
    assert!(
        text.contains("<f>A2*2+bonus</f><v>12</v>"),
        "formula with cached value"
    );
    assert!(
        text.contains("<f>A3-A2</f>"),
        "row references become sheet references"
    );
    assert!(text.contains("<definedName name=\"bonus\">10</definedName>"));
    assert!(text.contains("formatCode=\"#,##0.0\""));
    assert!(text.contains("xSplit=\"1\" ySplit=\"1\""));
    assert!(text.contains("<conditionalFormatting sqref=\"B2:B4\">"));
    assert!(
        text.contains("Grand Total"),
        "the pivot sheet carries the computed pivot"
    );
}

/// A column sized by `width=…dp` keeps the same characters in view when the
/// grid is zoomed: its pixel width follows the font size.
#[gpui::test]
fn dp_widths_follow_grid_zoom(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    view.update(cx, |view, cx| {
        view.set_query_sql(
            "-- sqlly format: column=name; width=40dp\nSELECT id, name FROM people",
            cx,
        );
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        let font = state.read(cx).font_size;
        let width = state.read(cx).data.columns[1].width;
        assert!(
            (width - crate::query_format::dp_to_px(40, font)).abs() < 0.6,
            "{width} at {font}"
        );
        view.adjust_grid_zoom(0.5, cx);
        let zoomed_font = state.read(cx).font_size;
        assert!(zoomed_font > font);
        let zoomed = state.read(cx).data.columns[1].width;
        assert!(
            (zoomed - crate::query_format::dp_to_px(40, zoomed_font)).abs() < 0.6,
            "{zoomed} at {zoomed_font}"
        );
    });
}

// ---------------------------------------------------------------------------
// Cell Details panel freshness, staged edits in the panel and transpose,
// column-width menu on the marker column
// ---------------------------------------------------------------------------

/// Attaches a Cell Details panel and focuses grid cell `(row, column)` of
/// slot 0 (grid coordinates, marker column included when shown).
fn attach_cell_detail_and_focus(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
    row: usize,
    column: usize,
) -> gpui::Entity<crate::cell_detail_panel::CellDetailPanel> {
    let panel = cx.new(crate::cell_detail_panel::CellDetailPanel::new);
    view.update(cx, |view, cx| {
        view.set_cell_detail(
            Some(panel.clone()),
            crate::cell_detail_model::DockSide::Right,
            300.0,
            cx,
        );
    });
    focus_grid_cell(view, cx, row, column);
    panel
}

fn focus_grid_cell(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
    row: usize,
    column: usize,
) {
    view.update(cx, |view, cx| {
        let grid = view.grid_views[0].as_ref().expect("grid").clone();
        grid.read(cx).state.clone().update(cx, |s, cx| {
            s.selection = sqlly_datatable::Selection::Cell(row, column);
            cx.notify();
        });
    });
    cx.run_until_parked();
}

fn panel_value(
    panel: &gpui::Entity<crate::cell_detail_panel::CellDetailPanel>,
    cx: &mut gpui::VisualTestContext,
) -> Option<CellValue> {
    panel.read_with(cx, |p, _| p.value_for_open())
}

#[gpui::test]
fn cell_detail_clears_when_a_rerun_leaves_no_focus(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 1);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alice".into()))
    );
    view.update(cx, |view, cx| {
        view.start_query(cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..2), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        panel_value(&panel, cx),
        None,
        "the new grid has no selection; the previous run's row must not linger"
    );
}

#[gpui::test]
fn cell_detail_clears_when_results_are_cleared(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 1);
    assert!(panel_value(&panel, cx).is_some());
    view.update(cx, |view, cx| view.clear(cx));
    cx.run_until_parked();
    assert_eq!(panel_value(&panel, cx), None);
}

#[gpui::test]
fn cell_detail_shows_the_staged_value_of_the_focused_cell(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    // Grid column 1 is `name` before the marker column appears.
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 1);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alice".into()))
    );
    stage_manual_name_edit(
        &view,
        cx,
        1,
        "Alice",
        crate::pending_changes::CellEdit::Value(CellValue::String("Alicia".into())),
    );
    cx.run_until_parked();
    // The marker column now leads; `name` is grid column 2.
    focus_grid_cell(&view, cx, 0, 2);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alicia".into())),
        "the panel shows what the grid paints"
    );
}

#[gpui::test]
fn cell_detail_refreshes_when_the_focused_cell_is_staged(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    // Focus `name` of row 1 with the marker column already showing (a staged
    // edit on another row), then stage an edit on the focused cell.
    stage_manual_name_edit(
        &view,
        cx,
        3,
        "Bob",
        crate::pending_changes::CellEdit::Value(CellValue::String("Robert".into())),
    );
    cx.run_until_parked();
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 2);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alice".into()))
    );
    stage_manual_name_edit(
        &view,
        cx,
        1,
        "Alice",
        crate::pending_changes::CellEdit::Value(CellValue::String("Alicia".into())),
    );
    cx.run_until_parked();
    focus_grid_cell(&view, cx, 0, 2);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alicia".into()))
    );
}

#[gpui::test]
fn cell_detail_shows_a_staged_inserted_row(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.apply_staged_op(
            0,
            replace_test_session(),
            crate::row_editor::StagedRowOp::Insert {
                values: vec![(
                    "name".to_string(),
                    crate::pending_changes::CellEdit::Value(CellValue::String("Zed".into())),
                )],
            },
            cx,
        );
    });
    cx.run_until_parked();
    // Three committed rows, then the staged insert; `name` is grid column 2.
    let panel = attach_cell_detail_and_focus(&view, cx, 3, 2);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Zed".into()))
    );
}

#[gpui::test]
fn transpose_with_staged_edits_keeps_columns_aligned(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    stage_manual_name_edit(
        &view,
        cx,
        1,
        "Alice",
        crate::pending_changes::CellEdit::Value(CellValue::String("Alicia".into())),
    );
    cx.run_until_parked();
    view.update(cx, |view, cx| view.toggle_transpose(0, cx));
    view.read_with(cx, |view, cx| {
        let state = view
            .transposed
            .first()
            .and_then(|g| g.as_ref())
            .expect("transposed grid")
            .read(cx)
            .state
            .read(cx);
        let cell_text = |row: usize, col: usize| match &state.data.rows[row][col] {
            sqlly_datatable::CellValue::Text(t) => t.to_string(),
            other => format!("{other:?}"),
        };
        assert_eq!(state.data.rows.len(), 2, "one row per result column");
        assert_eq!(cell_text(0, 0), "id");
        assert_eq!(cell_text(1, 0), "name");
        assert!(
            cell_text(0, 1).contains('1'),
            "id of row 1, got {}",
            cell_text(0, 1)
        );
        assert_eq!(cell_text(1, 1), "Alicia", "the staged value, not a shift");
        assert_eq!(cell_text(1, 3), "Bob");
    });
}

#[gpui::test]
fn column_width_menu_ignores_the_staged_edit_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    stage_manual_name_edit(
        &view,
        cx,
        1,
        "Alice",
        crate::pending_changes::CellEdit::Value(CellValue::String("Alicia".into())),
    );
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .clone();
        assert_eq!(view.grid_marker_width_of(0, state.read(cx)), 1);
        let before: Vec<f32> = state
            .read(cx)
            .data
            .columns
            .iter()
            .map(|c| c.width)
            .collect();
        let sentinel = Some(123.0_f32);
        if let Some(track) = view.auto_size.get_mut(&0)
            && let Some(first) = track.applied.first_mut()
        {
            *first = sentinel;
        }
        let auto_before = view.auto_size.get(&0).map(|t| t.applied.clone());
        view.adjust_column_width(0, 0, 40.0, cx);
        let after: Vec<f32> = state
            .read(cx)
            .data
            .columns
            .iter()
            .map(|c| c.width)
            .collect();
        assert_eq!(after, before, "the marker column keeps its width");
        assert_eq!(
            view.auto_size.get(&0).map(|t| t.applied.clone()),
            auto_before,
            "result column 0 keeps its auto-size"
        );
    });
}

// ---------------------------------------------------------------------------
// Editable Cell Details: the panel stages through the grid's cell-edit path
// ---------------------------------------------------------------------------

/// A Cell Details panel on `Items` (`id` key, nullable `name`) whose rows
/// the panel may edit, focused on grid cell `(row, column)`.
fn editable_cell_detail(
    cx: &mut gpui::TestAppContext,
    row: usize,
    column: usize,
) -> (
    gpui::Entity<ResultsView>,
    gpui::Entity<crate::cell_detail_panel::CellDetailPanel>,
    &mut gpui::VisualTestContext,
) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, _| {
        view.query_sql = Some("SELECT id, name FROM dbo.Items".to_string());
    });
    let panel = attach_cell_detail_and_focus(&view, cx, row, column);
    (view, panel, cx)
}

type SeenCellActions =
    std::rc::Rc<std::cell::RefCell<Vec<(Vec<CellValue>, usize, crate::cell_actions::CellAction)>>>;

/// Records the cell-menu style requests the results view sends its host.
fn capture_cell_actions(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
) -> SeenCellActions {
    let seen: SeenCellActions = Default::default();
    let sink = seen.clone();
    cx.update(|_window, cx| {
        cx.subscribe(view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::CellActionRequested {
                row,
                result_column,
                action,
                ..
            } = event
            {
                sink.borrow_mut()
                    .push((row.clone(), *result_column, action.clone()));
            }
        })
        .detach();
    });
    seen
}

/// What the host does with a cell action once the row's edit session is
/// open: parse the value for the table column and stage an update.
fn stage_like_the_host(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
    (row, result_column, action): (Vec<CellValue>, usize, crate::cell_actions::CellAction),
) {
    let session = sqlly_services::row_editing::RowEditSession {
        original_row: row.clone(),
        ..replace_test_session()
    };
    let column = session.columns[result_column].clone();
    let edit = match action {
        crate::cell_actions::CellAction::SetText(text) => {
            crate::cell_actions::text_cell_edit(sqlly_core::DatabaseType::SqlServer, &column, &text)
                .expect("valid text")
        }
        crate::cell_actions::CellAction::SetNull => {
            crate::pending_changes::CellEdit::Value(CellValue::Null)
        }
        other => panic!("unexpected action {other:?}"),
    };
    view.update(cx, |view, cx| {
        let columns = view.result_sets[0].columns.clone();
        let key = crate::pending_changes::row_key_from_result_row(&session, &columns, &row)
            .expect("keyed row");
        view.apply_staged_op(
            0,
            session,
            crate::row_editor::StagedRowOp::Update {
                key,
                original_row: row,
                changes: [(column.name.clone(), edit)].into_iter().collect(),
            },
            cx,
        );
    });
    cx.run_until_parked();
}

fn begin_panel_edit(
    panel: &gpui::Entity<crate::cell_detail_panel::CellDetailPanel>,
    cx: &mut gpui::VisualTestContext,
    draft: &str,
) {
    cx.update(|window, cx| {
        panel.update(cx, |p, cx| {
            p.begin_edit(window, cx);
            p.set_draft(draft, window, cx);
        });
    });
    cx.run_until_parked();
}

fn grid_text(
    view: &gpui::Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
    row: usize,
    col: usize,
) -> String {
    view.read_with(cx, |view, cx| {
        let state = view.grid_views[0]
            .as_ref()
            .expect("grid")
            .read(cx)
            .state
            .read(cx);
        match &state.data.rows[row][col] {
            sqlly_datatable::CellValue::Text(t) => t.to_string(),
            other => format!("{other:?}"),
        }
    })
}

const APPLY_CHORD: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};

#[gpui::test]
fn cell_detail_edit_stages_like_a_grid_cell_edit(cx: &mut gpui::TestAppContext) {
    let (view, panel, cx) = editable_cell_detail(cx, 0, 1);
    assert_eq!(
        panel.read_with(cx, |p, _| p.edit_gate().cloned()),
        Some(crate::cell_detail_model::CellEditGate::Editable)
    );
    let seen = capture_cell_actions(&view, cx);
    begin_panel_edit(&panel, cx, "Alicia");
    assert!(panel.read_with(cx, |p, _| p.is_editing()));
    cx.simulate_keystrokes(APPLY_CHORD);
    cx.run_until_parked();
    let request = seen
        .borrow_mut()
        .pop()
        .expect("the panel staged through the host");
    assert_eq!(
        request,
        (
            vec![CellValue::I64(1), CellValue::String("Alice".into())],
            1,
            crate::cell_actions::CellAction::SetText("Alicia".into()),
        ),
        "the shortcut applies the draft verbatim (no newline added)"
    );
    stage_like_the_host(&view, cx, request);
    // (Single-line: the grid's debug text shaping rejects newlines in any
    // cell, staged or fetched.) The grid paints the staged value (behind
    // the marker column) and the
    // panel stays on the edited cell, now showing it.
    assert_eq!(grid_text(&view, cx, 0, 2), "Alicia");
    assert!(!panel.read_with(cx, |p, _| p.is_editing()));
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alicia".into()))
    );
    // Save sees the staged change.
    let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
    let sink = saved.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::SaveStagedRequested { preview_sql, .. } = event {
                *sink.borrow_mut() = Some(preview_sql.clone());
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| view.save_staged(cx));
    cx.run_until_parked();
    let preview = saved.borrow().clone().expect("save requested");
    assert!(preview.contains("Alicia"), "{preview}");
}

#[gpui::test]
fn cell_detail_escape_reverts_without_staging(cx: &mut gpui::TestAppContext) {
    let (view, panel, cx) = editable_cell_detail(cx, 0, 1);
    let seen = capture_cell_actions(&view, cx);
    begin_panel_edit(&panel, cx, "scratch");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!panel.read_with(cx, |p, _| p.is_editing()));
    assert!(seen.borrow().is_empty());
    assert!(!view.read_with(cx, |view, _| view.has_staged_changes()));
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alice".into()))
    );
    // Reopening starts from the cell's value again, not the discarded draft.
    cx.update(|window, cx| panel.update(cx, |p, cx| p.begin_edit(window, cx)));
    panel.update(cx, |p, cx| p.apply_edit(cx));
    cx.run_until_parked();
    assert!(seen.borrow().is_empty(), "unchanged text stages nothing");
}

#[gpui::test]
fn cell_detail_keeps_null_and_empty_string_apart(cx: &mut gpui::TestAppContext) {
    // Row 2's name is NULL: applying an empty editor stages '' (not NULL).
    let (view, panel, cx) = editable_cell_detail(cx, 1, 1);
    assert_eq!(panel_value(&panel, cx), Some(CellValue::Null));
    let seen = capture_cell_actions(&view, cx);
    begin_panel_edit(&panel, cx, "");
    panel.update(cx, |p, cx| p.apply_edit(cx));
    cx.run_until_parked();
    let request = seen.borrow_mut().pop().expect("staged");
    assert_eq!(
        request.2,
        crate::cell_actions::CellAction::SetText(String::new())
    );
    stage_like_the_host(&view, cx, request);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String(String::new())),
        "an empty string, not NULL"
    );
    // Set NULL stages NULL, whatever the draft says.
    focus_grid_cell(&view, cx, 0, 2);
    begin_panel_edit(&panel, cx, "ignored");
    panel.update(cx, |p, cx| p.set_null_edit(cx));
    cx.run_until_parked();
    let request = seen.borrow_mut().pop().expect("staged");
    assert_eq!(request.2, crate::cell_actions::CellAction::SetNull);
    stage_like_the_host(&view, cx, request);
    assert_eq!(panel_value(&panel, cx), Some(CellValue::Null));
}

#[gpui::test]
fn cell_detail_is_read_only_with_a_reason(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, _| {
        view.query_sql = Some("SELECT count(*) FROM dbo.Items".to_string());
    });
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 1);
    let gate = panel.read_with(cx, |p, _| p.edit_gate().cloned());
    let Some(crate::cell_detail_model::CellEditGate::ReadOnly(reason)) = gate else {
        panic!("expected read-only, got {gate:?}");
    };
    assert!(!reason.is_empty());
    cx.update(|window, cx| panel.update(cx, |p, cx| p.begin_edit(window, cx)));
    assert!(!panel.read_with(cx, |p, _| p.is_editing()));
}

#[gpui::test]
fn cell_detail_waits_for_the_full_value_before_editing(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.query_sql = Some("SELECT id, name FROM dbo.Items".to_string());
        view.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: vec![sqlly_services::TruncatedCell {
                    row: 0,
                    column: 1,
                    total_len: 3 * 1024 * 1024,
                    kind: sqlly_services::TruncatedKind::Text,
                }],
            },
            cx,
        );
    });
    let panel = attach_cell_detail_and_focus(&view, cx, 0, 1);
    assert_eq!(
        panel.read_with(cx, |p, _| p.edit_gate().cloned()),
        Some(crate::cell_detail_model::CellEditGate::AwaitingFullValue)
    );
    cx.update(|window, cx| panel.update(cx, |p, cx| p.begin_edit(window, cx)));
    assert!(
        !panel.read_with(cx, |p, _| p.is_editing()),
        "a truncated preview is never edited"
    );
    view.update(cx, |view, cx| {
        view.apply_hydrated_cell_value(CellValue::String("Alice, in full".into()), cx)
    });
    assert_eq!(
        panel.read_with(cx, |p, _| p.edit_gate().cloned()),
        Some(crate::cell_detail_model::CellEditGate::Editable)
    );
    // A value that could not be fetched stays read-only.
    focus_grid_cell(&view, cx, 2, 1);
    view.update(cx, |view, cx| view.refresh_cell_detail(cx));
    focus_grid_cell(&view, cx, 0, 1);
    view.update(cx, |view, cx| view.refresh_cell_detail(cx));
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.apply_cell_hydration_unavailable("unavailable".into(), cx)
    });
    assert!(matches!(
        panel.read_with(cx, |p, _| p.edit_gate().cloned()),
        Some(crate::cell_detail_model::CellEditGate::ReadOnly(_))
    ));
}

#[gpui::test]
fn pinned_cell_detail_edits_the_pinned_cell(cx: &mut gpui::TestAppContext) {
    let (view, panel, cx) = editable_cell_detail(cx, 0, 1);
    panel.update(cx, |p, cx| p.toggle_pin_for_test(cx));
    focus_grid_cell(&view, cx, 2, 1);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Alice".into())),
        "pinned to row 1"
    );
    let seen = capture_cell_actions(&view, cx);
    begin_panel_edit(&panel, cx, "Pinned");
    panel.update(cx, |p, cx| p.apply_edit(cx));
    cx.run_until_parked();
    let request = seen.borrow_mut().pop().expect("staged");
    assert_eq!(request.0[0], CellValue::I64(1), "the pinned row, not row 3");
    stage_like_the_host(&view, cx, request);
    assert_eq!(
        panel_value(&panel, cx),
        Some(CellValue::String("Pinned".into())),
        "the pinned panel shows its staged value"
    );
}

/// Sequel Ace #2193: Apply to Query writes the grid's multi-column sort as
/// the statement's ORDER BY, replacing the one it had.
#[gpui::test]
fn apply_to_query_carries_the_grid_sort_as_order_by(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ResultsView::new(cx));
    let editor = cx.new_window_entity(|_window, cx| crate::editor::SqlEditor::new(cx));
    cx.update(|_window, app| {
        editor.update(app, |e, cx| {
            e.set_text("SELECT id, name FROM people ORDER BY id;", cx)
        });
    });
    view.update(cx, |view, cx| {
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(named_rows(&[(1, "b"), (2, "a"), (3, "b")]), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        view.set_editor(editor.downgrade(), cx);
        view.sort_column_descending(0, 1, cx);
        view.add_to_sort(0, 0, cx);
        view.open_filter_builder(0, cx);
        view.filter_builder_clear_rows(cx);
        view.apply_filter_builder_to_query(cx);
    });
    cx.run_until_parked();
    cx.update(|_window, app| {
        assert_eq!(
            editor.read(app).text().to_string(),
            "SELECT id, name FROM people ORDER BY [name] DESC, [id] ASC;"
        );
    });
}

/// A finished, spill-backed `total`-row result (`id`, `name = row{id}`).
fn spilled_results_view(
    cx: &mut gpui::TestAppContext,
    total: usize,
) -> (Entity<ResultsView>, &mut gpui::VisualTestContext, String) {
    // A folder of its own, so drill files written next to the spill file
    // can be counted without other tests' files in the way.
    let written = write_test_spill(total);
    let dir = std::env::temp_dir().join(format!(
        "sqlly-drill-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    let spill_file = dir
        .join(
            std::path::Path::new(&written)
                .file_name()
                .expect("spill file name"),
        )
        .to_string_lossy()
        .to_string();
    std::fs::rename(&written, &spill_file).expect("move spill");
    let _ = std::fs::rename(format!("{written}.idx"), format!("{spill_file}.idx"));
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.perf_prefs = crate::preferences::PerformancePreferences::default();
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(rows_batch(0..100), cx);
        v
    });
    cx.run_until_parked();
    let manifest_event = QueryEvent::SpillManifest {
        manifest: sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "name".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                row_count: total,
                spill_file: spill_file.clone(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: Some(1),
        },
    };
    view.update(cx, |v, cx| {
        v.handle_event(manifest_event, cx);
        v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    (view, cx, spill_file)
}

fn remove_test_spill(spill_file: &str) {
    let _ = std::fs::remove_file(spill_file);
    let _ = std::fs::remove_file(format!("{spill_file}.idx"));
    if let Some(dir) = std::path::Path::new(spill_file).parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Double-clicking a pivot cell over a spill-backed result writes exactly
/// that cell's rows to a filtered spill file and shows it under the drill
/// banner; the pivot keeps reading the full result, and Clear puts the full
/// result back and deletes the filtered file.
#[gpui::test]
fn pivot_drill_on_a_spilled_result_shows_a_filtered_spill_file(cx: &mut gpui::TestAppContext) {
    let total = 50_000usize;
    let (view, cx, spill_file) = spilled_results_view(cx, total);
    // Count ids per name: every name is unique, so each leaf drills to one row.
    view.update(cx, |v, cx| {
        let table = v.grid_views[0].clone().expect("grid exists");
        table.update(cx, |table, cx| {
            table.enable_pivot(
                sqlly_datatable::PivotConfig {
                    row_fields: vec![1],
                    value_field: Some(0),
                    aggregation: sqlly_datatable::AggregationFn::Count,
                    ..sqlly_datatable::PivotConfig::default()
                },
                cx,
            );
            table.set_active_tab(GridTab::Pivot, cx);
        });
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        let table = v.grid_views[0].clone().expect("grid exists");
        let pivot = table.read(cx).pivot_state().expect("pivot").clone();
        let first = pivot.read(cx).row_label(&pivot.read(cx).visible_rows()[0]);
        assert_eq!(first, "row0", "canonical order puts row0 first");
        pivot.update(cx, |p, cx| {
            p.request_drill_down(0, 0);
            cx.notify();
        });
        table.update(cx, |_t, cx| cx.notify());
    });
    cx.update(|window, _cx| window.refresh());
    cx.run_until_parked();

    let drill_file = view.read_with(cx, |v, cx| {
        assert!(v.spill_drill_shown(0), "the filtered view is shown");
        let backing = v.spill_backings[0].as_ref().expect("backing");
        assert_eq!(backing.total, 1, "exactly the drilled cell's rows");
        assert_ne!(backing.path, spill_file);
        assert!(std::path::Path::new(&backing.path).exists());
        let table = v.grid_views[0].as_ref().expect("grid").read(cx);
        assert_eq!(table.active_tab(), GridTab::Grid);
        assert_eq!(table.drill_filter(), Some("name = row0"));
        assert_eq!(
            table.state.read(cx).window.map(|w| w.total_rows),
            Some(1),
            "the grid windows over the filtered file"
        );
        assert_eq!(
            table.row_source().expect("source").row_count(),
            total,
            "the pivot still reads the full result"
        );
        backing.path.clone()
    });

    view.update(cx, |v, cx| v.clear_spill_drill(0, cx));
    view.read_with(cx, |v, cx| {
        assert!(!v.spill_drill_shown(0));
        assert_eq!(v.spill_backings[0].as_ref().expect("backing").total, total);
        let table = v.grid_views[0].as_ref().expect("grid").read(cx);
        assert_eq!(table.drill_filter(), None);
        assert_eq!(
            table.state.read(cx).window.map(|w| w.total_rows),
            Some(total)
        );
    });
    assert!(
        !std::path::Path::new(&drill_file).exists(),
        "Clear deletes the filtered file"
    );
    remove_test_spill(&spill_file);
}

/// Clearing for a new query drops the filtered view and deletes its file.
#[gpui::test]
fn new_query_deletes_the_drill_file(cx: &mut gpui::TestAppContext) {
    let (view, cx, spill_file) = spilled_results_view(cx, 20_000);
    view.update(cx, |v, cx| {
        v.start_spill_drill(
            0,
            sqlly_datatable::RowSourceDrill::new("id < 25", |row| {
                matches!(row.first(), Some(sqlly_datatable::CellValue::Integer(id)) if *id < 25)
            }),
            cx,
        );
    });
    cx.run_until_parked();
    let drill_file = view.read_with(cx, |v, _cx| {
        let backing = v.spill_backings[0].as_ref().expect("backing");
        assert_eq!(backing.total, 25);
        backing.path.clone()
    });
    assert!(std::path::Path::new(&drill_file).exists());
    view.update(cx, |v, cx| v.clear(cx));
    cx.run_until_parked();
    assert!(
        !std::path::Path::new(&drill_file).exists(),
        "clearing for a new query removes the filtered file"
    );
    view.read_with(cx, |v, _cx| assert!(!v.spill_drill_shown(0)));
    remove_test_spill(&spill_file);
}

/// A drill replaced by a newer one leaves nothing of the older on disk, and
/// only the newer lands.
#[gpui::test]
fn superseded_drill_leaves_no_file(cx: &mut gpui::TestAppContext) {
    let (view, cx, spill_file) = spilled_results_view(cx, 20_000);
    let dir = std::path::Path::new(&spill_file)
        .parent()
        .expect("dir")
        .to_path_buf();
    let drills_on_disk = || {
        std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        name.starts_with("sqlly-spill-drill-") && name.ends_with(".bin")
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(drills_on_disk(), 0);
    view.update(cx, |v, cx| {
        let id_below = |limit: i64| {
            sqlly_datatable::RowSourceDrill::new(format!("id < {limit}"), move |row| {
                matches!(row.first(), Some(sqlly_datatable::CellValue::Integer(id)) if *id < limit)
            })
        };
        v.start_spill_drill(0, id_below(10), cx);
        v.start_spill_drill(0, id_below(30), cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, _cx| {
        assert_eq!(v.spill_backings[0].as_ref().expect("backing").total, 30);
    });
    assert_eq!(drills_on_disk(), 1, "only the newest drill's file remains");
    view.update(cx, |v, cx| v.clear_spill_drill(0, cx));
    assert_eq!(drills_on_disk(), 0, "Clear removes it too");
    remove_test_spill(&spill_file);
}
