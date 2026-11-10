//! Grid-column vs result-column indexing while a staged-edit marker column
//! leads the grid, background column scans of windowed results, and the
//! column-layout store's handling of an unreadable file.

use super::*;
use crate::grid_filter_builder::{BuilderOp, ColumnTarget, Conjunction, FilterRow};
use sqlly_services::row_editing::{RowEditColumn, RowEditSession, TableTarget};

fn edit_column(name: &str, data_type: &str, ordinal: usize, key: bool) -> RowEditColumn {
    RowEditColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        char_max_length: None,
        precision: None,
        scale: None,
        nullable: !key,
        is_primary_key: key,
        is_identity: key,
        is_computed: false,
        is_rowversion: false,
        default_definition: None,
        result_ordinal: Some(ordinal),
        enum_values: Vec::new(),
    }
}

fn items_session() -> RowEditSession {
    RowEditSession {
        target: TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        },
        key_columns: vec!["id".to_string()],
        rowversion_column: None,
        columns: vec![
            edit_column("id", "int", 0, true),
            edit_column("name", "varchar", 1, false),
        ],
        original_row: vec![CellValue::I64(1), CellValue::String("Alice".into())],
        foreign_keys: Vec::new(),
        fk_display: Vec::new(),
        check_constraints: Vec::new(),
        incoming_foreign_keys: Vec::new(),
    }
}

/// Stage one insert on slot 0 so the grid rebuilds with the marker column.
fn stage_insert(view: &mut ResultsView, cx: &mut Context<ResultsView>) {
    view.apply_staged_op(
        0,
        items_session(),
        crate::row_editor::StagedRowOp::Insert {
            values: vec![(
                "id".to_string(),
                crate::pending_changes::CellEdit::Value(CellValue::I64(99)),
            )],
        },
        cx,
    );
    assert_eq!(
        view.marker_offset(0),
        1,
        "the insert stages a marker column"
    );
}

fn grid_state(view: &ResultsView, cx: &App) -> Entity<GridState> {
    view.grid_views[0]
        .as_ref()
        .expect("grid")
        .read(cx)
        .state
        .clone()
}

fn row(column: usize, op: BuilderOp, a: &str) -> FilterRow {
    FilterRow {
        column: ColumnTarget::Index(column),
        op,
        a: a.into(),
        b: String::new(),
        enabled: true,
    }
}

/// name contains "o" OR id = 1 — a tree, so it runs client side.
fn or_tree(view: &mut ResultsView) {
    let builder = view.filter_builder.as_mut().expect("builder open");
    builder.model.root.conjunction = Conjunction::Or;
    builder.model.clear_items();
    builder.model.push_row(row(1, BuilderOp::Contains, "o"));
    builder.model.push_row(row(0, BuilderOp::Eq, "1"));
}

fn source_map(view: &ResultsView) -> Option<Vec<usize>> {
    view.source_index_map
        .first()
        .and_then(|map| map.as_ref())
        .map(|map| map.as_ref().clone())
}

#[gpui::test]
fn filter_builder_lists_result_columns_while_edits_are_staged(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        view.open_filter_builder(0, cx);
        let builder = view.filter_builder.as_ref().expect("builder");
        assert_eq!(builder.column_names, vec!["id", "name"]);
        assert_eq!(builder.column_kinds.len(), 2);
        // Apply to Query renders against the same result columns.
        let mut model = builder.model.clone();
        model.clear_items();
        model.push_row(row(1, BuilderOp::Contains, "Bob"));
        let clause = model
            .to_where_clause(
                sqlly_lang::sqlmodel::DatabaseType::SqlServer,
                &builder.result_columns,
                &builder.column_kinds,
            )
            .expect("clause");
        assert!(clause.contains("[name]"), "{clause}");
        assert!(clause.contains("Bob"), "{clause}");
    });
}

#[gpui::test]
fn flat_filter_targets_the_result_column_behind_the_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        view.open_filter_builder(0, cx);
        let builder = view.filter_builder.as_mut().expect("builder");
        builder.model.clear_items();
        builder.model.push_row(row(1, BuilderOp::Contains, "Bob"));
        view.apply_filter_builder(cx);
        let state = grid_state(view, cx);
        let state = state.read(cx);
        assert_eq!(state.filters.len(), 3, "marker + two result columns");
        assert!(
            !state.filters[0].is_active(),
            "the marker column is never filtered"
        );
        assert!(
            !state.filters[1].is_active(),
            "id is not the filtered column"
        );
        assert!(state.filters[2].is_active(), "name carries the predicate");
        assert_eq!(state.display_indices.len(), 1, "only Bob's row matches");
    });
    // Reopening seeds the same result-column model back.
    view.update(cx, |view, cx| {
        view.close_filter_builder(cx);
        view.open_filter_builder(0, cx);
        let rows = view
            .filter_builder
            .as_ref()
            .expect("builder")
            .model
            .flat_rows()
            .len();
        assert_eq!(rows, 1);
        let first = view.filter_builder.as_ref().unwrap().model.flat_rows()[0].clone();
        assert_eq!(first.column, ColumnTarget::Index(1));
    });
}

#[gpui::test]
fn tree_filter_evaluates_result_columns_with_the_marker_present(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        view.open_filter_builder(0, cx);
        or_tree(view);
        view.apply_filter_builder(cx);
        // Alice (id 1) and Bob (contains "o"); the NULL-name row fails both.
        assert_eq!(source_map(view), Some(vec![0, 2]));
    });
}

#[gpui::test]
fn tree_filter_survives_staging_an_edit(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.open_filter_builder(0, cx);
        or_tree(view);
        view.apply_filter_builder(cx);
        assert_eq!(source_map(view), Some(vec![0, 2]));
        // Staging drops and rebuilds the grid with the marker column; the
        // tree filter must still apply to the rebuilt grid.
        stage_insert(view, cx);
        assert_eq!(source_map(view), Some(vec![0, 2]));
    });
}

#[gpui::test]
fn staged_marker_does_not_shift_column_formats(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let before = view.update(cx, |view, cx| {
        let state = grid_state(view, cx);
        let formats = state.read(cx).resolved_formats.clone();
        formats.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>()
    });
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        let state = grid_state(view, cx);
        let formats = state.read(cx).resolved_formats.clone();
        let after: Vec<String> = formats.iter().map(|f| format!("{f:?}")).collect();
        assert_eq!(after.len(), 3);
        assert_eq!(after[1..], before[..], "each column keeps its own format");
    });
}

#[gpui::test]
fn copying_a_selected_column_skips_the_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        let state = grid_state(view, cx);
        // Grid column 2 is the name column once the marker leads the grid.
        state.update(cx, |s, _| {
            s.selection = sqlly_datatable::Selection::Column(2)
        });
        view.copy_selection_default(0, false, cx);
    });
    let text = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(text.contains("Alice") && text.contains("Bob"), "{text:?}");
    assert!(!text.contains('\t'), "one column only: {text:?}");
}

#[gpui::test]
fn layout_saved_while_edits_are_staged_is_in_result_columns(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        view.set_layout_store_for_test(crate::column_layout::ColumnLayoutStore::default());
        stage_insert(view, cx);
        let state = grid_state(view, cx);
        state.update(cx, |s, _| s.data.columns[2].width = 333.0);
        view.note_possible_layout_change(0, cx);
        let generation = view.layout_save_generation[0];
        view.flush_layout_save(0, generation, cx);
        let key = crate::column_layout::key_for("h", "d", "dbo", "select * from t");
        let sig = crate::column_layout::signature_for(&view.result_sets[0].columns);
        let layout = view
            .layout_store
            .as_ref()
            .and_then(|store| store.layout_for(&key, &sig))
            .expect("a layout the next run can use");
        assert_eq!(layout.widths.len(), 2);
        assert_eq!(layout.widths[1], 333.0, "name's width, not the marker's");
        assert!(layout.order.iter().all(|&i| i < 2), "{:?}", layout.order);
    });
}

#[test]
fn selection_in_result_columns_drops_the_marker() {
    use sqlly_datatable::Selection;
    assert_eq!(
        selection_in_result_columns(&Selection::Column(2), 1),
        Selection::Column(1)
    );
    assert_eq!(
        selection_in_result_columns(&Selection::Column(0), 1),
        Selection::None
    );
    assert_eq!(
        selection_in_result_columns(&Selection::Columns(vec![0, 1, 2]), 1),
        Selection::Columns(vec![0, 1])
    );
    assert_eq!(
        selection_in_result_columns(&Selection::CellRange(0, 0, 3, 2), 1),
        Selection::CellRange(0, 0, 3, 1)
    );
    assert_eq!(
        selection_in_result_columns(&Selection::Cells(vec![(0, 0), (1, 2)]), 1),
        Selection::Cells(vec![(1, 1)])
    );
    assert_eq!(
        selection_in_result_columns(&Selection::Row(4), 1),
        Selection::Row(4)
    );
    assert_eq!(
        selection_in_result_columns(&Selection::Column(2), 0),
        Selection::Column(2)
    );
}

#[test]
fn grid_marker_width_reads_the_grid_not_pending_state() {
    let marker = crate::grid_bridge::marker_column();
    let id = sqlly_datatable::Column {
        name: "id".into(),
        kind: ColumnKind::Integer,
        width: 80.0,
    };
    assert_eq!(grid_marker_width(&[marker.clone(), id.clone()], 1), 1);
    assert_eq!(grid_marker_width(std::slice::from_ref(&id), 1), 0);
    // A result column that happens to be named like the marker is not one.
    assert_eq!(grid_marker_width(&[marker.clone(), id], 2), 0);
}

// --- background column scans (Copy Distinct / Profile on windowed sets) ---

fn spill_view(
    cx: &mut gpui::TestAppContext,
    total: usize,
) -> (Entity<ResultsView>, &mut gpui::VisualTestContext) {
    let spill_file = write_test_spill(total);
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let mut v = ResultsView::new(cx);
        v.handle_event(QueryEvent::Started, cx);
        v.handle_event(two_column_result_set(), cx);
        v.handle_event(
            QueryEvent::SpillWindow {
                index: 0,
                spill_file: spill_file.clone(),
                rows_available: total,
            },
            cx,
        );
        v
    });
    cx.run_until_parked();
    (view, cx)
}

fn clipboard(cx: &mut gpui::VisualTestContext) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

#[gpui::test]
fn copy_distinct_on_a_windowed_result_reads_in_the_background(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 25);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    view.update(cx, |view, cx| {
        let started =
            view.start_column_scan(0, 1, ColumnScanPurpose::CopyDistinct { top: None }, cx);
        assert!(started, "a windowed slot scans in the background");
        // Nothing was read on this (UI) thread yet: the caption says it is
        // counting and the clipboard is untouched.
        let caption = view.grid_caption(0).unwrap_or_default();
        assert!(
            caption.contains("Counting distinct values in name"),
            "{caption}"
        );
    });
    assert_eq!(clipboard(cx), "sentinel");
    cx.run_until_parked();
    // Identical to the synchronous copy: every distinct value in order.
    let expected: Vec<CellValue> = (0..25)
        .map(|i| CellValue::String(format!("row{i}")))
        .collect();
    assert_eq!(
        clipboard(cx),
        crate::result_grid_menu::distinct_column_lines(&expected)
    );
    view.read_with(cx, |view, _| {
        assert!(view.background_read.is_none());
        assert!(view.grid_caption(0).is_none());
    });
}

#[gpui::test]
fn a_newer_column_scan_supersedes_the_older_one(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 10);
    view.update(cx, |view, cx| {
        view.start_column_scan(0, 0, ColumnScanPurpose::CopyDistinct { top: None }, cx);
        view.start_column_scan(0, 1, ColumnScanPurpose::CopyDistinct { top: None }, cx);
    });
    cx.run_until_parked();
    let text = clipboard(cx);
    assert!(
        text.starts_with("row0"),
        "the newer scan (name) wins: {text:?}"
    );
}

#[gpui::test]
fn a_column_scan_of_a_replaced_result_copies_nothing(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 10);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    view.update(cx, |view, cx| {
        view.start_column_scan(0, 1, ColumnScanPurpose::CopyDistinct { top: None }, cx);
        view.clear(cx);
    });
    cx.run_until_parked();
    assert_eq!(clipboard(cx), "sentinel");
}

#[gpui::test]
fn column_scan_is_refused_for_a_resident_result(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        assert!(!view.start_column_scan(0, 1, ColumnScanPurpose::Profile, cx));
        assert!(view.background_read.is_none());
    });
}

#[test]
fn read_spill_column_matches_a_whole_row_read() {
    let path = write_test_spill(4_500);
    let column = read_spill_column(&path, 1, 4_200).expect("readable");
    assert_eq!(column.len(), 4_200, "clamped to the requested count");
    assert_eq!(column[0], CellValue::String("row0".into()));
    assert_eq!(column[4_199], CellValue::String("row4199".into()));
    // Past the end of the file: the rows it has.
    assert_eq!(
        read_spill_column(&path, 0, 10_000).map(|c| c.len()),
        Some(4_500)
    );
    assert!(read_spill_column("/definitely/not/a/spill/file", 0, 1).is_none());
    crate::spill_pager::remove_spill_files(&path);
}

// --- column-layout store: an unreadable file is surfaced, never replaced ---

#[gpui::test]
fn corrupt_layout_store_is_reported_and_left_untouched(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let _storage = crate::host_storage::test_support::redirect_storage(dir.path().to_path_buf());
    let path = dir
        .path()
        .join("data")
        .join(crate::column_layout::ColumnLayoutStore::FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"entries\": [broken").unwrap();

    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        view.set_query_sql("select * from t", cx);
        view.set_layout_scope(Some(crate::column_layout::LayoutScope {
            server: "h".into(),
            database: "d".into(),
            default_schema: "dbo".into(),
            ..Default::default()
        }));
        // Re-run so the grid build consults the (corrupt) store.
        view.clear(cx);
        view.handle_event(two_column_result_set(), cx);
        view.handle_event(rows_batch(0..3), cx);
        view.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        assert!(
            view.messages
                .iter()
                .any(|m| m.contains("Column layouts could not be loaded")),
            "{:?}",
            view.messages
        );
        let state = grid_state(view, cx);
        state.update(cx, |s, _| s.data.columns[0].width = 250.0);
        view.note_possible_layout_change(0, cx);
        let generation = view.layout_save_generation[0];
        view.flush_layout_save(0, generation, cx);
        view.reset_column_layout(0, cx);
    });
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"{\"entries\": [broken",
        "the user's file is never overwritten"
    );
}

// --- grid exports of cells fetched as previews ---

#[gpui::test]
fn grid_export_says_when_cells_are_previews(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        assert!(!view.note_preview_cells_in_export(0, cx), "nothing was cut");
        view.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: vec![sqlly_services::TruncatedCell {
                    row: 0,
                    column: 1,
                    total_len: 20 * 1024,
                    kind: sqlly_services::TruncatedKind::Text,
                }],
            },
            cx,
        );
        assert!(view.note_preview_cells_in_export(0, cx));
        let note = view.messages.last().cloned().unwrap_or_default();
        assert!(note.contains("1 cell in this result"), "{note}");
        assert!(note.contains("Export Tables"), "{note}");
    });
}

// --- grid-menu actions on a grid that leads with the marker column ---

/// A cell request on grid column 2 (name) of a marker grid, row 0.
fn marker_cell_request() -> sqlly_datatable::ContextMenuRequest {
    use sqlly_datatable::{
        CellValue as G, ColumnContext, ColumnKind as K, ContextMenuRequest, ContextMenuTarget,
    };
    let column = |index: usize, name: &str, kind: K| ColumnContext {
        index,
        name: name.into(),
        kind,
    };
    ContextMenuRequest::for_test(
        ContextMenuTarget::Cell {
            display_row_index: 0,
            source_row_index: 0,
            column_index: 2,
        },
        None,
        vec![vec![G::None, G::Integer(1), G::Text("Alice".into())]],
        vec![
            column(0, crate::grid_bridge::MARKER_COLUMN_NAME, K::Text),
            column(1, "id", K::Integer),
            column(2, "name", K::Text),
        ],
    )
}

/// Run a grid-menu action through the provider, inside the grid's own
/// update like the real context menu.
fn run_menu_action(view: &Entity<ResultsView>, action: &str, cx: &mut gpui::VisualTestContext) {
    use sqlly_datatable::ContextMenuProvider as _;
    let provider = crate::result_grid_menu::ResultGridMenuProvider::new(view.downgrade(), None, 0);
    let state = view.read_with(cx, grid_state);
    let request = marker_cell_request();
    cx.update(|_window, cx| {
        state.update(cx, |state, cx| {
            provider.on_action(action, &request, state, cx)
        });
    });
}

#[gpui::test]
fn open_referenced_row_names_the_result_column_behind_the_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = seen.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::FollowForeignKeyRequested { column_index, .. } = event {
                sink.borrow_mut().push(*column_index);
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id, name FROM Items", cx);
        stage_insert(view, cx);
    });
    run_menu_action(
        &view,
        crate::result_grid_menu::action_id::FK_OPEN_REFERENCED,
        cx,
    );
    assert_eq!(
        *seen.borrow(),
        vec![1],
        "grid column 2 is result column 1 (name)"
    );
}

#[gpui::test]
fn cell_actions_target_the_result_column_behind_the_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = seen.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if let ResultsViewEvent::CellActionRequested {
                column_name,
                result_column,
                ..
            } = event
            {
                sink.borrow_mut()
                    .push((column_name.clone(), *result_column));
            }
        })
        .detach();
    });
    view.update(cx, |view, cx| {
        view.set_query_sql("SELECT id, name FROM Items", cx);
        stage_insert(view, cx);
    });
    run_menu_action(&view, crate::result_grid_menu::action_id::CELL_SET_NULL, cx);
    assert_eq!(*seen.borrow(), vec![("name".to_string(), 1)]);
}

#[gpui::test]
fn cell_details_menu_item_selects_the_cell_and_shows_the_panel(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    let opened = std::rc::Rc::new(std::cell::Cell::new(false));
    let sink = opened.clone();
    cx.update(|_window, cx| {
        cx.subscribe(&view, move |_view, event: &ResultsViewEvent, _cx| {
            if matches!(event, ResultsViewEvent::CellDetailOpenRequested) {
                sink.set(true);
            }
        })
        .detach();
    });
    view.update(cx, stage_insert);
    run_menu_action(&view, crate::result_grid_menu::action_id::CELL_DETAILS, cx);
    cx.run_until_parked();
    let selection = view.read_with(cx, |view, cx| {
        grid_state(view, cx).read(cx).selection.clone()
    });
    assert_eq!(selection, sqlly_datatable::Selection::Cell(0, 2));
    assert!(opened.get(), "a hidden panel asks the host to show it");
}

#[gpui::test]
fn cell_details_focus_is_a_result_column_behind_the_marker(cx: &mut gpui::TestAppContext) {
    let (view, cx) = make_results_view_with_data(cx);
    view.update(cx, |view, cx| {
        stage_insert(view, cx);
        let state = grid_state(view, cx);
        state.update(cx, |s, _| {
            s.selection = sqlly_datatable::Selection::Cell(0, 2)
        });
        let selection = selection_in_result_columns(
            &state.read(cx).selection,
            view.grid_marker_width_of(0, state.read(cx)),
        );
        let focus =
            crate::cell_detail_model::focused_cell(&selection, &state.read(cx).display_indices)
                .expect("a focused cell");
        assert_eq!(
            focus.column, 1,
            "the name column, not the marker's neighbour"
        );
    });
}

// --- export/snapshot/profile reads of windowed results happen off the UI thread ---

#[gpui::test]
fn deferred_spill_prefix_is_identical_to_a_synchronous_read(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 30);
    let limit = 25;
    let ran = std::rc::Rc::new(std::cell::Cell::new(false));
    let sync = view.read_with(cx, |view, _| {
        view.spill_backings[0]
            .as_ref()
            .expect("windowed")
            .first_rows(limit)
    });
    view.update(cx, |view, cx| {
        let flag = ran.clone();
        let deferred = view.defer_until_spill_prefixes(
            vec![(0, limit)],
            "Reading…".to_string(),
            cx,
            move |_| flag.set(true),
        );
        assert!(deferred, "a windowed slot waits for its rows");
        assert_eq!(view.grid_caption(0).as_deref(), Some("Reading…"));
        assert!(view.spill_prefixes.borrow().is_empty(), "nothing read yet");
    });
    assert!(!ran.get());
    cx.run_until_parked();
    assert!(ran.get(), "the action re-runs once the rows are in memory");
    view.update(cx, |view, cx| {
        // The waiting rows are the synchronous read, and the next ask is
        // satisfied from memory without deferring again.
        assert!(!view.defer_until_spill_prefixes(vec![(0, limit)], String::new(), cx, |_| {}));
        let (rows, truncated) = view.spill_rows_for_export(0, limit).expect("windowed");
        assert_eq!(rows, sync);
        assert!(truncated, "30 rows, 25 read");
        assert!(view.spill_prefixes.borrow().is_empty(), "taken on use");
        assert!(view.grid_caption(0).is_none());
    });
}

#[gpui::test]
fn a_deferred_spill_read_of_a_replaced_result_never_reruns(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 10);
    let ran = std::rc::Rc::new(std::cell::Cell::new(false));
    view.update(cx, |view, cx| {
        let flag = ran.clone();
        view.defer_until_spill_prefixes(vec![(0, 5)], String::new(), cx, move |_| flag.set(true));
        view.clear(cx);
    });
    cx.run_until_parked();
    assert!(!ran.get());
}

#[gpui::test]
fn copying_a_windowed_result_reads_it_in_the_background(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 25);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    view.update(cx, |view, cx| {
        view.copy_selection_default(0, true, cx);
        let caption = view.grid_caption(0).unwrap_or_default();
        assert!(
            caption.starts_with("Copying — reading 25 rows"),
            "{caption}"
        );
    });
    assert_eq!(
        clipboard(cx),
        "sentinel",
        "nothing read on the UI thread yet"
    );
    cx.run_until_parked();
    let text = clipboard(cx);
    assert!(text.contains("row0") && text.contains("row24"), "{text}");
}

#[gpui::test]
fn grid_menu_exports_of_a_windowed_result_rerun_after_the_read(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 25);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    {
        use sqlly_datatable::ContextMenuProvider as _;
        let provider =
            crate::result_grid_menu::ResultGridMenuProvider::new(view.downgrade(), None, 0);
        let state = view.read_with(cx, grid_state);
        let request = sqlly_datatable::ContextMenuRequest::for_test(
            sqlly_datatable::ContextMenuTarget::ColumnHeader { column_index: 1 },
            None,
            Vec::new(),
            Vec::new(),
        );
        cx.update(|_window, cx| {
            state.update(cx, |state, cx| {
                provider.on_action(
                    crate::result_grid_menu::action_id::COPY_AS_CSV,
                    &request,
                    state,
                    cx,
                )
            });
        });
    }
    assert_eq!(clipboard(cx), "sentinel");
    cx.run_until_parked();
    let text = clipboard(cx);
    assert!(text.contains("row24"), "{text}");
}

#[gpui::test]
fn profiling_a_windowed_result_reads_it_in_the_background(cx: &mut gpui::TestAppContext) {
    let (view, cx) = spill_view(cx, 12);
    view.update(cx, |view, cx| {
        view.open_result_profile(0, cx);
        assert!(
            view.result_profile.is_none(),
            "profiled after the read, not now"
        );
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.result_profile.is_some());
    });
}
