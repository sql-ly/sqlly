//! Foreign-key label discovery, fetch, and grid paint wiring for [`ResultsView`].

use std::collections::HashMap;

use gpui::{Context, Entity};
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_services::Services;
use sqlly_services::row_editing::{RowEditOpenOutcome, RowEditOpenRequest};

use crate::column_layout::{self, single_table_target};
use crate::fk_labels::{LabelledFk, labelled_fks, plan_requests};
use crate::grid_bridge::apply_fk_value_labels;
use sqlly_datatable::SqllyDataTable;

use super::ResultsView;

const LAYOUT_VERSION: u32 = 1;

/// Per-column FK label menu state captured when the grid provider is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FkLabelHeaderMenuInfo {
    pub local_column: String,
    pub display_column: String,
    pub column_label_on: bool,
    pub show_fk_labels: bool,
}

#[derive(Debug, Clone)]
pub(super) struct FkSlotState {
    /// FK columns currently painted (per-column off entries removed).
    pub fks: Vec<LabelledFk>,
    /// All resolvable FKs before the per-column off filter — used by the header menu.
    pub discovered_fks: Vec<LabelledFk>,
    pub generation: u64,
}

impl ResultsView {
    pub fn set_fk_label_context(
        &mut self,
        profile: Option<ConnectionProfile>,
        services: Option<Services>,
    ) {
        self.fk_label_profile = profile;
        self.fk_label_services = services;
    }

    pub fn clear_fk_label_cache(&mut self) {
        self.fk_label_cache.clear();
        self.fk_labels_by_slot.clear();
    }

    pub(super) fn fk_labels_enabled(&self) -> bool {
        self.formatting_or_load().show_fk_labels
    }

    pub(super) fn fk_label_mode(&self) -> crate::preferences::FkLabelMode {
        self.formatting_or_load().fk_label_mode
    }

    fn fk_label_overrides(&self, slot: usize) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some(layout) = self.applied_layouts.get(slot).and_then(|l| l.as_ref()) else {
            return out;
        };
        for (column, display) in &layout.fk_labels {
            if let Some(display) = display {
                out.insert(column.clone(), display.clone());
            }
        }
        out
    }

    /// Snapshot for the result-grid header menu (`menu_items` has no `&App`).
    pub(super) fn fk_label_header_snapshot(
        &self,
        slot: usize,
    ) -> std::collections::HashMap<usize, FkLabelHeaderMenuInfo> {
        let mut out = std::collections::HashMap::new();
        let Some(state) = self.fk_labels_by_slot.get(slot).and_then(|s| s.as_ref()) else {
            return out;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return out;
        };
        let show_fk_labels = self.fk_labels_enabled();
        for fk in &state.discovered_fks {
            let Some(col) = rs.columns.get(fk.column_index) else {
                continue;
            };
            let column_label_on = state
                .fks
                .iter()
                .any(|active| active.column_index == fk.column_index);
            out.insert(
                fk.column_index,
                FkLabelHeaderMenuInfo {
                    local_column: col.name.clone(),
                    display_column: fk.display_column.clone(),
                    column_label_on,
                    show_fk_labels,
                },
            );
        }
        out
    }

    pub fn set_fk_label_column(
        &mut self,
        slot: usize,
        local_column: &str,
        display_column: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Guard the slot like every sibling layout method (e.g.
        // `reset_column_layout`); the direct `result_sets[slot]` indexes below
        // would otherwise panic if the slot were ever stale.
        if slot >= self.result_sets.len() {
            return;
        }
        while self.applied_layouts.len() <= slot {
            self.applied_layouts.push(None);
        }
        let sig = column_layout::signature_for(&self.result_sets[slot].columns);
        let mut layout = self
            .applied_layouts
            .get(slot)
            .and_then(|l| l.clone())
            .unwrap_or_else(|| column_layout::ColumnLayout {
                version: LAYOUT_VERSION,
                signature: sig.clone(),
                widths: self.result_sets[slot]
                    .columns
                    .iter()
                    .map(|_| 120.0)
                    .collect(),
                order: (0..self.result_sets[slot].columns.len()).collect(),
                hidden: vec![],
                frozen: 0,
                // Layouts store result columns; the view's spec follows the
                // grid, which leads with the marker column while edits are
                // staged.
                sort: column_layout::shift_sort_spec(
                    &self.sort_spec_for_slot(slot),
                    -(self.marker_offset(slot) as isize),
                ),
                fk_labels: Vec::new(),
            });
        if let Some(entry) = layout.fk_labels.iter_mut().find(|(c, _)| c == local_column) {
            entry.1 = display_column.clone();
        } else {
            layout
                .fk_labels
                .push((local_column.to_string(), display_column));
        }
        self.applied_layouts[slot] = Some(layout.clone());
        if let Some(key) = self.layout_key_for_slot(slot) {
            self.update_layout_store(|store| store.upsert(key, layout));
        }
        self.rediscover_fk_labels(slot, cx);
        self.note_possible_layout_change(slot, cx);
    }

    pub fn set_show_fk_labels(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut prefs = self.formatting_or_load();
        prefs.show_fk_labels = enabled;
        self.formatting = Some(prefs.clone());
        crate::preferences::PreferencesStore::new().set_result_formatting(prefs);
        if enabled {
            for slot in 0..self.result_sets.len() {
                self.rediscover_fk_labels(slot, cx);
            }
        } else {
            self.clear_fk_label_cache();
            self.refresh_all_fk_label_paint(cx);
        }
    }

    pub(super) fn maybe_start_fk_labels(&mut self, slot: usize, cx: &mut Context<Self>) {
        if !self.fk_labels_enabled() || self.layout_scope.is_none() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        if single_table_target(&sql).is_none() {
            return;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if rs.rows.is_empty() {
            return;
        }
        self.rediscover_fk_labels(slot, cx);
    }

    pub(super) fn rediscover_fk_labels(&mut self, slot: usize, cx: &mut Context<Self>) {
        if !self.fk_labels_enabled() || self.layout_scope.is_none() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        if single_table_target(&sql).is_none() {
            return;
        }
        let Some(profile) = self.fk_label_profile.clone() else {
            return;
        };
        let Some(services) = self.fk_label_services.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if rs.rows.is_empty() {
            return;
        }
        while self.fk_label_fetch_generation.len() <= slot {
            self.fk_label_fetch_generation.push(0);
        }
        self.fk_label_fetch_generation[slot] = self.fk_label_fetch_generation[slot].wrapping_add(1);
        let generation = self.fk_label_fetch_generation[slot];
        while self.fk_labels_by_slot.len() <= slot {
            self.fk_labels_by_slot.push(None);
        }
        self.fk_labels_by_slot[slot] = None;

        let columns = rs.columns.clone();
        let row = rs.rows[0].clone();
        let request = RowEditOpenRequest {
            sql,
            result_set_index: slot,
            columns: columns.clone(),
            row,
        };
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let outcome = crate::tokio_handle()
                .spawn(async move {
                    services
                        .row_editing
                        .open_insert_session(&profile, request)
                        .await
                })
                .await;
            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| {
                if this.fk_label_fetch_generation.get(slot) != Some(&generation) {
                    return;
                }
                let overrides = this.fk_label_overrides(slot);
                let mut fks = match outcome {
                    Ok(Ok(RowEditOpenOutcome::Editable(session))) => {
                        labelled_fks(&session.foreign_keys, &columns, &overrides)
                    }
                    _ => Vec::new(),
                };
                let discovered_fks = fks.clone();
                if let Some(layout) = this.applied_layouts.get(slot).and_then(|l| l.as_ref()) {
                    let disabled: std::collections::HashSet<&str> = layout
                        .fk_labels
                        .iter()
                        .filter(|(_, display)| display.is_none())
                        .map(|(column, _)| column.as_str())
                        .collect();
                    fks.retain(|fk| {
                        columns
                            .get(fk.column_index)
                            .is_some_and(|c| !disabled.contains(c.name.as_str()))
                    });
                }
                this.fk_labels_by_slot[slot] = Some(FkSlotState {
                    fks,
                    discovered_fks,
                    generation,
                });
                this.fetch_fk_labels(slot, generation, cx);
            });
        })
        .detach();
    }

    fn fetch_fk_labels(&mut self, slot: usize, generation: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.fk_label_profile.clone() else {
            return;
        };
        let Some(services) = self.fk_label_services.clone() else {
            return;
        };
        let Some(state) = self.fk_labels_by_slot.get(slot).and_then(|s| s.as_ref()) else {
            return;
        };
        if state.generation != generation {
            return;
        }
        let rows = self
            .result_sets
            .get(slot)
            .map(|rs| rs.rows.clone())
            .unwrap_or_default();
        let mut batches = Vec::new();
        for fk in &state.fks {
            let prefix = format!(
                "{}|{}|{}",
                fk.referenced.schema, fk.referenced.table, fk.key_column
            );
            for request in plan_requests(fk, &rows, &self.fk_label_cache) {
                batches.push((prefix.clone(), request));
            }
        }
        if batches.is_empty() {
            self.refresh_fk_label_paint(slot, cx);
            return;
        }
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let mut collected = Vec::new();
            for (prefix, request) in batches {
                let result = crate::tokio_handle()
                    .spawn({
                        let profile = profile.clone();
                        let services = services.clone();
                        async move { services.row_editing.fk_labels(&profile, request).await }
                    })
                    .await;
                if let Ok(Ok(result)) = result {
                    collected.push((prefix, result.labels));
                }
            }
            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| {
                if this.fk_label_fetch_generation.get(slot) != Some(&generation) {
                    return;
                }
                for (prefix, labels) in collected {
                    this.fk_label_cache.insert_all(&prefix, labels);
                }
                this.refresh_fk_label_paint(slot, cx);
            });
        })
        .detach();
    }

    pub(super) fn apply_fk_labels_to_config(
        &mut self,
        config: &mut sqlly_datatable::GridConfig,
        slot: usize,
        rows: &[Vec<CellValue>],
    ) {
        let Some(state) = self.fk_labels_by_slot.get(slot).and_then(|s| s.as_ref()) else {
            return;
        };
        if state.fks.is_empty() {
            return;
        }
        let mode = self.fk_label_mode().to_datatable();
        apply_fk_value_labels(config, &state.fks, rows, &mut self.fk_label_cache, mode);
    }

    fn refresh_fk_label_paint(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(Some(grid)) = self.grid_views.get(slot).cloned() else {
            return;
        };
        self.refresh_fk_label_paint_on_grid(slot, grid, cx);
    }

    #[allow(dead_code)]
    fn refresh_all_fk_label_paint(&mut self, cx: &mut Context<Self>) {
        for slot in 0..self.grid_views.len() {
            if let Some(Some(grid)) = self.grid_views.get(slot).cloned() {
                self.refresh_fk_label_paint_on_grid(slot, grid, cx);
            }
        }
    }

    fn refresh_fk_label_paint_on_grid(
        &mut self,
        slot: usize,
        grid: Entity<SqllyDataTable>,
        cx: &mut Context<Self>,
    ) {
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let rows = self.owned_rows_for_grid(slot, cx);
        let mut config = self.grid_config_for_columns(slot, &columns);
        self.apply_fk_labels_to_config(&mut config, slot, &rows);
        let state = grid.read(cx).state.clone();
        let marker = self.grid_marker_width_of(slot, state.read(cx));
        super::align_config_to_marker(&mut config, marker);
        state.update(cx, |s, cx| {
            s.config = config;
            s.resolved_formats = std::sync::Arc::new(s.config.resolve_all(&s.data.columns));
            cx.notify();
        });
    }
}
