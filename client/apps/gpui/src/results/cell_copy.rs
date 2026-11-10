//! Double-click a results-grid cell to copy its value.
//!
//! The gesture copies exactly what Copy (Cmd+C) copies for a single selected
//! cell: the cell's raw value through the same display formatter
//! (`NULL`, `0x…` hex for binary, the engine's text for dates), with the
//! slot's export masks applied first so a masked column never leaks its
//! unmasked value. A cell whose value arrived as a truncated preview is
//! fetched in full first, as Copy does — unless the column is masked, where
//! the masked preview is all the user is entitled to.

use super::*;
use gpui_component::WindowExt as _;

/// The brief toast shown after a double-click copy. Typed so a burst of
/// double-clicks replaces one toast instead of stacking several.
pub(super) struct ValueCopiedToast;

/// What a double-click copy did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CellCopyOutcome {
    /// The value is on the clipboard.
    Copied(String),
    /// The cell holds a truncated preview; its full value is being fetched
    /// and the hydration path copies it (and reports it) when it lands.
    FetchingFullValue,
    /// Nothing to copy: no single result cell is selected.
    Nothing,
}

/// Whether a left double-click landed on a data cell of the flat grid, judged
/// from the grid's own bookkeeping after it processed the second mouse-down.
///
/// `click_hit` must be a cell and the selection must be that same single
/// cell (a shift/cmd-extended or multi-cell selection is not a "this value"
/// gesture). The pointer must also sit inside the cell area: the row-number
/// gutter's View/Edit icons consume their mouse-down without updating
/// `click_hit`, so a double-click there would otherwise read a stale cell.
pub(crate) fn is_cell_double_click(
    click_hit: Option<sqlly_datatable::HitResult>,
    selection: &sqlly_datatable::Selection,
    pointer_in_grid: gpui::Point<gpui::Pixels>,
    row_header_width: f32,
    header_height: f32,
) -> bool {
    let Some(sqlly_datatable::HitResult::Cell(row, col)) = click_hit else {
        return false;
    };
    if *selection != sqlly_datatable::Selection::Cell(row, col) {
        return false;
    }
    f32::from(pointer_in_grid.x) >= row_header_width
        && f32::from(pointer_in_grid.y) >= header_height
}

/// Whether `col` carries an export mask under `prefs` — the same test
/// [`crate::result_cell_format::apply_export_masks`] applies.
pub(crate) fn column_export_masked(
    columns: &[ResultColumn],
    col: usize,
    prefs: &ResultFormattingPreferences,
) -> bool {
    prefs.exports_masked
        && columns.get(col).is_some_and(|c| {
            prefs
                .column_overrides
                .get(&c.name)
                .is_some_and(|f| f.kind == ResultKind::Mask)
        })
}

/// The clipboard text for one cell of a result row: export masks applied,
/// then the same display formatter Copy uses for a single cell.
pub(crate) fn cell_copy_text(
    columns: &[ResultColumn],
    row: Vec<CellValue>,
    col: usize,
    prefs: &ResultFormattingPreferences,
) -> Option<String> {
    let masked = crate::result_cell_format::apply_export_masks(columns, vec![row], prefs);
    masked
        .first()
        .and_then(|r| r.get(col))
        .map(result_export::cell_display)
}

impl ResultsView {
    /// Handle a left mouse-up on a slot's grid container: on a double-click
    /// over a data cell of the Grid tab, copy that cell's value and flash a
    /// short "Value copied" toast. The Pivot tab keeps its own double-click
    /// (drill-through) and the Chart tab has no cells, so both are left alone.
    pub(super) fn handle_grid_double_click(
        &mut self,
        slot: usize,
        ev: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.click_count < 2 || ev.modifiers.modified() {
            return;
        }
        let Some(view) = self.grid_views.get(slot).and_then(|g| g.clone()) else {
            return;
        };
        if view.read(cx).active_tab() != GridTab::Grid {
            return;
        }
        let on_cell = {
            let s = view.read(cx).state.read(cx);
            !s.is_busy()
                && is_cell_double_click(
                    s.click_hit,
                    &s.selection,
                    gpui::point(
                        ev.position.x - s.bounds.origin.x,
                        ev.position.y - s.bounds.origin.y,
                    ),
                    s.row_header_width,
                    s.header_height,
                )
        };
        if !on_cell {
            return;
        }
        if let CellCopyOutcome::Copied(_) = self.copy_focused_cell_value(slot, cx) {
            window.push_notification(
                gpui_component::notification::Notification::success(crate::localization::text(
                    "results.cell_copy.value_copied",
                ))
                .id::<ValueCopiedToast>(),
                cx,
            );
        }
    }

    /// Copy the slot's single selected cell to the clipboard, formatted and
    /// masked exactly as Copy formats a single cell. Reads only that cell's
    /// row (one pager read on a spilled set) rather than the whole set.
    pub(crate) fn copy_focused_cell_value(
        &mut self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> CellCopyOutcome {
        let Some(state) = self.grid_state_for_slot(slot, cx) else {
            return CellCopyOutcome::Nothing;
        };
        let (selection, display_indices, window_offset) = {
            let s = state.read(cx);
            // The staged-edit marker column holds no value to copy.
            let marker = self.grid_marker_width_of(slot, s);
            (
                selection_in_result_columns(&s.selection, marker),
                s.display_indices.clone(),
                s.window.map(|w| w.offset),
            )
        };
        if !matches!(selection, sqlly_datatable::Selection::Cell(..)) {
            return CellCopyOutcome::Nothing;
        }
        let Some(focus) = crate::cell_detail_model::focused_cell(&selection, &display_indices)
        else {
            return CellCopyOutcome::Nothing;
        };
        let source_row = window_offset.map_or(focus.source_row, |offset| {
            offset.saturating_add(focus.source_row)
        });
        let Some(row) = self.source_row_for_slot(slot, source_row, cx) else {
            return CellCopyOutcome::Nothing;
        };
        let Some(columns) = self.result_set_at(slot).map(|rs| rs.columns.clone()) else {
            return CellCopyOutcome::Nothing;
        };
        let prefs = self.effective_formatting_for_slot(slot);
        let trunc_row = self.truncation_row_for_grid(slot, focus.source_row, window_offset);
        if !column_export_masked(&columns, focus.column, &prefs)
            && self.truncated_cell(slot, trunc_row, focus.column).is_some()
        {
            self.request_cell_hydration(
                slot,
                trunc_row,
                focus.column,
                crate::cell_hydration::HydratePurpose::Copy,
                cx,
            );
            return CellCopyOutcome::FetchingFullValue;
        }
        let Some(text) = cell_copy_text(&columns, row, focus.column, &prefs) else {
            return CellCopyOutcome::Nothing;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        CellCopyOutcome::Copied(text)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use sqlly_datatable::{HitResult, Selection};

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    fn masked_prefs(column: &str) -> ResultFormattingPreferences {
        let mut prefs = ResultFormattingPreferences {
            exports_masked: true,
            ..Default::default()
        };
        prefs.column_overrides.insert(
            column.to_string(),
            crate::preferences::ColumnFormat::default_for_kind(ResultKind::Mask),
        );
        prefs
    }

    fn at(x: f32, y: f32) -> gpui::Point<gpui::Pixels> {
        gpui::point(gpui::px(x), gpui::px(y))
    }

    #[test]
    fn double_click_on_the_selected_cell_counts() {
        assert!(is_cell_double_click(
            Some(HitResult::Cell(3, 1)),
            &Selection::Cell(3, 1),
            at(120.0, 80.0),
            40.0,
            24.0,
        ));
    }

    #[test]
    fn double_click_off_a_cell_does_not_count() {
        for hit in [
            None,
            Some(HitResult::ColumnHeader(1)),
            Some(HitResult::ColumnBorder(1)),
            Some(HitResult::RowHeader(3)),
            Some(HitResult::VerticalScrollbar),
            Some(HitResult::None),
        ] {
            assert!(
                !is_cell_double_click(hit, &Selection::Cell(3, 1), at(120.0, 80.0), 40.0, 24.0),
                "{hit:?}"
            );
        }
    }

    #[test]
    fn double_click_needs_that_single_cell_selected() {
        let hit = Some(HitResult::Cell(3, 1));
        assert!(!is_cell_double_click(
            hit,
            &Selection::CellRange(1, 1, 3, 1),
            at(120.0, 80.0),
            40.0,
            24.0
        ));
        assert!(!is_cell_double_click(
            hit,
            &Selection::Cell(2, 1),
            at(120.0, 80.0),
            40.0,
            24.0
        ));
        assert!(!is_cell_double_click(
            hit,
            &Selection::None,
            at(120.0, 80.0),
            40.0,
            24.0
        ));
    }

    #[test]
    fn double_click_in_the_gutter_or_header_band_does_not_count() {
        // The gutter's View/Edit icons leave a stale `click_hit` behind.
        let hit = Some(HitResult::Cell(3, 1));
        assert!(!is_cell_double_click(
            hit,
            &Selection::Cell(3, 1),
            at(20.0, 80.0),
            40.0,
            24.0
        ));
        assert!(!is_cell_double_click(
            hit,
            &Selection::Cell(3, 1),
            at(120.0, 10.0),
            40.0,
            24.0
        ));
    }

    #[test]
    fn copy_text_uses_the_copy_display_formatter() {
        let columns = vec![col("a", "int"), col("b", "varbinary"), col("c", "varchar")];
        let prefs = ResultFormattingPreferences::default();
        let row = vec![
            CellValue::Null,
            CellValue::Bytes(vec![0xde, 0xad]),
            CellValue::String("2026-10-22 10:00:00".into()),
        ];
        assert_eq!(
            cell_copy_text(&columns, row.clone(), 0, &prefs).as_deref(),
            Some("NULL")
        );
        assert_eq!(
            cell_copy_text(&columns, row.clone(), 1, &prefs).as_deref(),
            Some("0xDEAD")
        );
        assert_eq!(
            cell_copy_text(&columns, row.clone(), 2, &prefs).as_deref(),
            Some("2026-10-22 10:00:00")
        );
        assert_eq!(cell_copy_text(&columns, row, 9, &prefs), None);
    }

    #[test]
    fn copy_text_of_a_masked_column_never_holds_the_raw_value() {
        let columns = vec![col("id", "int"), col("secret", "text")];
        let prefs = masked_prefs("secret");
        assert!(column_export_masked(&columns, 1, &prefs));
        assert!(!column_export_masked(&columns, 0, &prefs));
        let row = vec![CellValue::I64(1), CellValue::String("hunter2-abcd".into())];
        let text = cell_copy_text(&columns, row.clone(), 1, &prefs).unwrap();
        assert_ne!(text, "hunter2-abcd");
        assert!(!text.contains("hunter2"), "{text:?}");
        assert_eq!(
            cell_copy_text(&columns, row, 0, &prefs).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn masking_off_for_exports_is_not_a_masked_column() {
        let columns = vec![col("secret", "text")];
        let mut prefs = masked_prefs("secret");
        prefs.exports_masked = false;
        assert!(!column_export_masked(&columns, 0, &prefs));
    }

    #[gpui::test]
    fn copy_focused_cell_value_copies_the_selected_cell(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            let mut v = ResultsView::new(cx);
            v.handle_event(
                QueryEvent::ResultSet {
                    index: 0,
                    columns: vec![col("id", "int"), col("name", "varchar")],
                },
                cx,
            );
            v.handle_event(
                QueryEvent::Rows {
                    index: 0,
                    rows: vec![
                        vec![CellValue::I64(1), CellValue::String("Alice".into())],
                        vec![CellValue::I64(2), CellValue::Null],
                    ],
                },
                cx,
            );
            v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
            v
        });
        cx.run_until_parked();
        let set_selection = |view: &Entity<ResultsView>, cx: &mut gpui::VisualTestContext, sel| {
            view.update(cx, |v, cx| {
                let state = v.grid_state_for_slot(0, cx).expect("grid");
                state.update(cx, |s, _| s.selection = sel);
            });
        };
        let clipboard = |cx: &mut gpui::VisualTestContext| {
            cx.read_from_clipboard()
                .as_ref()
                .and_then(|c| c.text())
                .unwrap_or_default()
                .to_string()
        };

        set_selection(&view, cx, Selection::Cell(0, 1));
        let outcome = view.update(cx, |v, cx| v.copy_focused_cell_value(0, cx));
        assert_eq!(outcome, CellCopyOutcome::Copied("Alice".into()));
        assert_eq!(clipboard(cx), "Alice");

        set_selection(&view, cx, Selection::Cell(1, 1));
        let outcome = view.update(cx, |v, cx| v.copy_focused_cell_value(0, cx));
        assert_eq!(outcome, CellCopyOutcome::Copied("NULL".into()));
        assert_eq!(clipboard(cx), "NULL");

        // A range is not "this value": nothing is copied.
        set_selection(&view, cx, Selection::CellRange(0, 0, 1, 1));
        let outcome = view.update(cx, |v, cx| v.copy_focused_cell_value(0, cx));
        assert_eq!(outcome, CellCopyOutcome::Nothing);
        assert_eq!(clipboard(cx), "NULL");
    }
}
