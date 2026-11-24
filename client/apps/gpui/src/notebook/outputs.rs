//! The notebook's output registry: the one place non-SQL cells read
//! upstream data from (contract #5).
//!
//! When a SQL cell's run completes, the view snapshots the first result set
//! of its results view into a [`CellOutput`]: the columns, the rows up to
//! the cell's output cap (an `Arc`, so readers never copy them), how
//! complete that snapshot is, and where the data came from. A spilled
//! result's rows are read from its spill file off the UI thread first
//! (`ResultsView::defer_until_spill_prefixes`), so a large result never
//! stalls the notebook. Local cells (charts, transforms, KPIs, …) publish
//! their own outputs here too, and every publish bumps the cell's version,
//! which is how dependents know to recompute.
//!
//! Readers check the producing cell's state with
//! `CellState::is_presentable_as_current()` before presenting an output as
//! live; the registry keeps the last output of a stale or failed cell so it
//! can still be shown, labelled.
//!
//! The registry also holds the notebook's cross-filter selections: a click
//! in a chart cell sets one keyed by that chart, and cells that follow the
//! chart filter their own input rows locally. Nothing here ever runs a
//! query.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use sqlly_core::ConnectionEnvironment;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::notebook::cells::spec::CrossFilter;
use sqlly_services::notebook::{CellOutput, Completeness, Fingerprint, Freshness, Provenance};

use crate::results::ResultsView;

/// Rows a SQL cell's output keeps when its policy sets no `max_rows`.
pub const DEFAULT_SNAPSHOT_ROW_CAP: usize = 100_000;

#[derive(Default)]
pub struct OutputRegistry {
    outputs: HashMap<String, Arc<CellOutput>>,
    versions: HashMap<String, u64>,
    generation: u64,
    filters: BTreeMap<String, CrossFilter>,
    filter_generation: u64,
    /// The row selected in a table cell's grid, as an index into that
    /// cell's output rows.
    selected_rows: HashMap<String, usize>,
}

impl OutputRegistry {
    /// Store `output` as `cell_id`'s current output; returns its version.
    pub fn publish(&mut self, cell_id: &str, output: CellOutput) -> u64 {
        self.generation += 1;
        self.outputs.insert(cell_id.to_string(), Arc::new(output));
        self.versions.insert(cell_id.to_string(), self.generation);
        self.generation
    }

    pub fn get(&self, cell_id: &str) -> Option<Arc<CellOutput>> {
        self.outputs.get(cell_id).cloned()
    }

    /// Changes whenever `cell_id` publishes; 0 before its first output.
    pub fn version(&self, cell_id: &str) -> u64 {
        self.versions.get(cell_id).copied().unwrap_or(0)
    }

    /// Changes on every publish and removal in the notebook.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn remove(&mut self, cell_id: &str) -> bool {
        self.versions.remove(cell_id);
        self.filters.remove(cell_id);
        self.selected_rows.remove(cell_id);
        let removed = self.outputs.remove(cell_id).is_some();
        if removed {
            self.generation += 1;
        }
        removed
    }

    /// Set the selection made in chart `filter.source` (replacing its last).
    pub fn set_filter(&mut self, filter: CrossFilter) {
        if self.filters.get(&filter.source) != Some(&filter) {
            self.filters.insert(filter.source.clone(), filter);
            self.filter_generation += 1;
        }
    }

    pub fn clear_filter(&mut self, source: &str) -> bool {
        let cleared = self.filters.remove(source).is_some();
        if cleared {
            self.filter_generation += 1;
        }
        cleared
    }

    /// The selection made in chart `source`.
    pub fn filter(&self, source: &str) -> Option<&CrossFilter> {
        self.filters.get(source)
    }

    pub fn filter_generation(&self) -> u64 {
        self.filter_generation
    }

    /// Note the row selected in table cell `cell_id` (`None` clears it).
    pub fn select_row(&mut self, cell_id: &str, row: Option<usize>) {
        match row {
            Some(row) => self.selected_rows.insert(cell_id.to_string(), row),
            None => self.selected_rows.remove(cell_id),
        };
    }

    #[cfg(test)]
    pub fn selected_row(&self, cell_id: &str) -> Option<usize> {
        self.selected_rows.get(cell_id).copied()
    }
}

/// A result set read out of a results view.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSnapshot {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    /// Rows the query produced.
    pub logical_rows: u64,
    /// The rows were read back from a spill file.
    pub spilled: bool,
}

/// How complete `rows_read` rows of a `logical_rows`-row result are under
/// `cap`, and the row count to report. A spilled result read back in full
/// is `Spilled`; one with more rows than the cap (or whose rows stopped
/// streaming into memory) is `Truncated` at what was kept.
pub fn snapshot_completeness(
    rows_read: usize,
    logical_rows: u64,
    spilled: bool,
    cap: usize,
) -> (Completeness, u64) {
    let total = logical_rows.max(rows_read as u64);
    let kept = rows_read.min(cap) as u64;
    if kept < total {
        (Completeness::Truncated { cap: kept }, total)
    } else if spilled {
        (Completeness::Spilled { total }, total)
    } else {
        (Completeness::Complete, total)
    }
}

/// The first result set of `results`, up to `cap` rows. For a spilled set
/// the rows come from the prefix `defer_until_spill_prefixes` read ahead
/// (or, when nobody asked ahead, straight from the file).
pub fn read_results(results: &ResultsView, cap: usize) -> Option<ResultSnapshot> {
    let set = results.result_set_at(0)?;
    if let Some((rows, _more)) = results.spill_rows_for_export(0, cap) {
        let logical = results
            .spill_scan_total(0)
            .unwrap_or(rows.len())
            .max(set.row_count_hint);
        return Some(ResultSnapshot {
            columns: set.columns.clone(),
            rows,
            logical_rows: logical as u64,
            spilled: true,
        });
    }
    let rows = set.rows.iter().take(cap).cloned().collect();
    Some(ResultSnapshot {
        columns: set.columns.clone(),
        rows,
        logical_rows: set.rows.len().max(set.row_count_hint) as u64,
        spilled: false,
    })
}

/// Where a result came from, for [`Provenance`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSource {
    pub connection_label: String,
    pub database: Option<String>,
    pub environment: Option<ConnectionEnvironment>,
    pub sql: Option<String>,
}

pub fn environment_name(environment: ConnectionEnvironment) -> &'static str {
    match environment {
        ConnectionEnvironment::Production => "production",
        ConnectionEnvironment::Staging => "staging",
        ConnectionEnvironment::Test => "test",
        ConnectionEnvironment::Dev => "development",
        ConnectionEnvironment::Local => "local",
        ConnectionEnvironment::Unset => "unknown",
    }
}

pub fn provenance(source: &ResultSource, now: chrono::DateTime<chrono::Utc>) -> Provenance {
    Provenance {
        connection_label: source.connection_label.clone(),
        database: source.database.clone(),
        environment: source.environment.map(|e| environment_name(e).to_string()),
        extracted_at: Some(now),
        sql_hash: source
            .sql
            .as_deref()
            .map(sqlly_services::local_analytics::source_sql_hash),
        sources: Vec::new(),
    }
}

/// A cell output from a snapshot, capped at `cap` rows.
pub fn output_from_snapshot(
    snapshot: ResultSnapshot,
    cap: usize,
    run_id: String,
    fingerprint: Fingerprint,
    provenance: Provenance,
    elapsed_ms: u64,
) -> CellOutput {
    let (completeness, row_count) = snapshot_completeness(
        snapshot.rows.len(),
        snapshot.logical_rows,
        snapshot.spilled,
        cap,
    );
    let mut rows = snapshot.rows;
    rows.truncate(cap);
    CellOutput {
        columns: snapshot.columns,
        rows: Arc::new(rows),
        completeness,
        row_count,
        elapsed_ms,
        run_id,
        fingerprint,
        provenance,
        freshness: Freshness::Live,
    }
}

/// An output computed in the notebook from other outputs (charts,
/// transforms, KPIs): complete only when every input was.
pub fn derived_output(
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    inputs: &[&CellOutput],
    run_id: String,
    fingerprint: Fingerprint,
) -> CellOutput {
    let completeness = inputs
        .iter()
        .map(|o| o.completeness)
        .find(|c| !c.supports_summaries())
        .unwrap_or(Completeness::Complete);
    let provenance = Provenance {
        connection_label: String::new(),
        database: None,
        environment: None,
        extracted_at: Some(chrono::Utc::now()),
        sql_hash: None,
        sources: inputs.iter().map(|o| o.provenance.clone()).collect(),
    };
    CellOutput {
        row_count: rows.len() as u64,
        columns,
        rows: Arc::new(rows),
        completeness,
        elapsed_ms: 0,
        run_id,
        fingerprint,
        provenance,
        freshness: Freshness::Live,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
        }
    }

    fn snapshot(rows: usize, logical: u64, spilled: bool) -> ResultSnapshot {
        ResultSnapshot {
            columns: vec![col("n")],
            rows: (0..rows as i64).map(|i| vec![CellValue::I64(i)]).collect(),
            logical_rows: logical,
            spilled,
        }
    }

    fn output(snapshot: ResultSnapshot, cap: usize) -> CellOutput {
        output_from_snapshot(
            snapshot,
            cap,
            "r_1".into(),
            Fingerprint("f".into()),
            Provenance::default(),
            3,
        )
    }

    #[test]
    fn snapshots_are_capped_and_say_how_complete_they_are() {
        let small = output(snapshot(5, 5, false), 10);
        assert_eq!(small.completeness, Completeness::Complete);
        assert_eq!(small.rows.len(), 5);

        let capped = output(snapshot(25, 25, false), 10);
        assert_eq!(capped.rows.len(), 10);
        assert_eq!(capped.row_count, 25);
        assert_eq!(capped.completeness, Completeness::Truncated { cap: 10 });

        // Rows stopped streaming into memory: the grid holds fewer than ran.
        let suppressed = output(snapshot(4, 9_000, false), 10);
        assert_eq!(suppressed.completeness, Completeness::Truncated { cap: 4 });
        assert_eq!(suppressed.row_count, 9_000);

        let spilled_whole = output(snapshot(8, 8, true), 10);
        assert_eq!(
            spilled_whole.completeness,
            Completeness::Spilled { total: 8 }
        );
        assert!(spilled_whole.completeness.supports_summaries());

        let spilled_over = output(snapshot(10, 400_000, true), 10);
        assert_eq!(
            spilled_over.completeness,
            Completeness::Truncated { cap: 10 }
        );
        assert!(!spilled_over.completeness.supports_summaries());
        assert_eq!(spilled_over.row_count, 400_000);
    }

    #[test]
    fn the_registry_versions_outputs_and_shares_rows() {
        let mut registry = OutputRegistry::default();
        assert_eq!(registry.version("a"), 0);
        let v1 = registry.publish("a", output(snapshot(3, 3, false), 10));
        let first = registry.get("a").unwrap();
        let again = registry.get("a").unwrap();
        assert!(Arc::ptr_eq(&first.rows, &again.rows));
        let v2 = registry.publish("a", output(snapshot(4, 4, false), 10));
        assert!(v2 > v1);
        assert_eq!(registry.version("a"), v2);
        assert_eq!(first.rows.len(), 3, "an older reader keeps its snapshot");
        registry.publish("b", output(snapshot(1, 1, false), 10));
        assert!(registry.remove("a"));
        assert!(registry.get("a").is_none());
        assert!(registry.get("b").is_some());
    }

    #[test]
    fn filters_are_kept_per_chart_and_count_changes() {
        let mut registry = OutputRegistry::default();
        let filter = CrossFilter {
            source: "chart".into(),
            column: "region".into(),
            value: "West".into(),
            grain: None,
        };
        registry.set_filter(filter.clone());
        let generation = registry.filter_generation();
        registry.set_filter(filter.clone());
        assert_eq!(registry.filter_generation(), generation, "same selection");
        assert_eq!(registry.filter("chart"), Some(&filter));
        assert!(registry.clear_filter("chart"));
        assert!(!registry.clear_filter("chart"));
        assert!(registry.filter("chart").is_none());
    }

    #[test]
    fn derived_outputs_inherit_incompleteness_and_provenance() {
        let mut upstream = output(snapshot(25, 25, false), 10);
        upstream.provenance.connection_label = "Sales".into();
        let derived = derived_output(
            vec![col("x")],
            vec![vec![CellValue::I64(1)]],
            &[&upstream],
            "r_2".into(),
            Fingerprint("g".into()),
        );
        assert_eq!(derived.completeness, Completeness::Truncated { cap: 10 });
        assert_eq!(derived.provenance.sources[0].connection_label, "Sales");
        let source = ResultSource {
            connection_label: "Sales".into(),
            database: Some("Northwind".into()),
            environment: Some(ConnectionEnvironment::Production),
            sql: Some("SELECT 1".into()),
        };
        let p = provenance(&source, chrono::Utc::now());
        assert_eq!(p.environment.as_deref(), Some("production"));
        assert!(p.sql_hash.unwrap().starts_with("sha256:"));
    }
}
