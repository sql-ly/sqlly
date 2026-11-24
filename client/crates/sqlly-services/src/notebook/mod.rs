//! Notebook core: the `.sqllynb` document model and everything that reasons
//! about it without running anything. Pure (no gpui, no I/O except the
//! native [`run_record::JsonlRunStore`]) and compiled for wasm too, so the
//! app, the CLI runner and the browser build share one model.
//!
//! - [`document`]: `NotebookDoc` v2, cells, kinds, ids, canonical JSON.
//! - [`migrate`]: loading `.sqlnb.json` v1 and `.ipynb`, the version chain,
//!   and writing `.ipynb`.
//! - [`references`]: `{{name}}` / `{{param.name}}` references, renames and
//!   name validation.
//! - [`expand`]: inlining referenced SQL cells as CTEs.
//! - [`graph`]: the dependency DAG over cells and parameters.
//! - [`fingerprint`]: cache keys per cell.
//! - [`plan`]: turning a run request into ordered, grouped steps.
//! - [`state`]: the per-cell output state machine and staleness.
//! - [`params`]: typed parameters, presets, literal rendering and binding.
//! - [`output`] and [`run_record`]: cell outputs and immutable run records.
//! - [`cells`]: typed `spec` accessors per cell kind.
//!
//! Errors carry a `notebook.core.*` message key with an English fallback
//! (`message()`), localized by the app.

/// `<data dir>/SQLLY/notebooks`: the one folder every notebook store (recovery
/// copies, run records, trust, cache, audit, local DuckDB workspaces) lives in.
///
/// `SQLLY_NOTEBOOK_DATA_DIR` overrides it; test builds set it so no test can
/// write recovery copies, trust records or caches into the real folder.
#[cfg(not(target_arch = "wasm32"))]
pub fn data_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("SQLLY_NOTEBOOK_DATA_DIR").filter(|d| !d.is_empty()) {
        return std::path::PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SQLLY")
        .join("notebooks")
}

pub mod cells;
pub mod display;
pub mod document;
pub mod editing;
pub mod expand;
pub mod fingerprint;
pub mod graph;
mod lenient;
mod memo;
pub mod migrate;
pub mod output;
pub mod params;
pub mod plan;
pub mod references;
pub mod run_details;
pub mod run_record;
mod scan;
pub mod state;
pub mod text_store;

pub use document::{
    CachePolicy, Cell, CellKind, ConnectionRef, DataRetention, ExecClass, ExecutionPolicy,
    FileFormat, IdSource, NotebookDoc, NotebookSettings, RandomIds, RunMode, SeededIds,
    SourceLanguage,
};
pub use expand::{ExpandError, ExpandOptions, ExpandedSql, Expansion, expand_sql};
pub use fingerprint::{
    CellFingerprint, ConnectionIdentity, Fingerprint, FingerprintInputs, fingerprint_all,
};
pub use graph::{CycleError, Graph, Node};
pub use migrate::{LoadError, Loaded, load, save, to_ipynb};
pub use output::{CellOutput, Completeness, Freshness, Provenance};
pub use params::{ParamError, ParamType, ParamValue, ParameterDef, ResolvedParams};
pub use plan::{PlanContext, RunPlan, RunRequest, Schedule, plan};
pub use references::Dialects;
#[cfg(not(target_arch = "wasm32"))]
pub use run_record::JsonlRunStore;
pub use run_record::{MemoryRunStore, RunRecord, RunStore};
pub use state::{CellEvent, CellState, CellStatus, NotebookState};
pub use text_store::{MemoryTextStore, TextRunStore, TextStore};
