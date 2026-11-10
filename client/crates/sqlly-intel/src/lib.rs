//! Everything that reasons *about* SQL and the objects it names: database
//! metadata and DDL, completion and hover, query classification and safety,
//! and the pooled model engine that keeps a live snapshot of a connection's
//! schema.
//!
//! Each module here was previously its own crate, and the layering between
//! them is unchanged: `metadata` and `intellisense` sit on `sqlly-core` plus
//! the language layer, `query` sits on `intellisense`, and `modelengine`
//! sits on `metadata`.
//!
//! `plans` parses captured execution plans (SQL Server ShowPlan XML and the
//! EXPLAIN output of the other engines) into one model and runs the findings
//! engine over it. It is pure text analysis with no I/O, so the desktop app
//! (through `sqlly_services::plans`) and the free online plan viewer
//! (`apps/planview-wasm`) share it.
//!
//! `query`'s dialect-invariant surfaces (safety classification, parameter
//! scanning, paren diagnostics) must never reach the language layer's
//! dialect-aware lexer — see ADR-016 and `tests/adr016_guard.rs`.

pub mod intellisense;
pub mod matching;
pub mod metadata;
pub mod natural_date;
pub mod plans;
pub mod query;
pub mod redis_complete;

// Native-only: nothing in the browser build reaches the model engine or the
// verification suite, and keeping them off wasm keeps a native rusqlite and
// tokio's full feature set out of that target's dependency graph.
#[cfg(not(target_arch = "wasm32"))]
pub mod intellisense_verify;
#[cfg(not(target_arch = "wasm32"))]
pub mod modelengine;

// The model engine's cursor-context ladder and fuzzy scorer are pure text
// analysis that the shared `intellisense` completions call into, so wasm gets
// that one submodule without the rest of the engine.
#[cfg(target_arch = "wasm32")]
pub mod modelengine {
    #[path = "intellisense/mod.rs"]
    pub mod intellisense;
}
