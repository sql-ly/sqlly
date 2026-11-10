//! Connection safety policy: SELECT-only classification, dangerous-artifact
//! detection, Include Statistics wrap handling, rollback wrapping, T-SQL
//! batch-lead rules, and policy application.
//!
//! All of it reads SQL through the security lexer (`sqlly_lang::seclex`):
//! per execution unit the driver sends, under every lexical reading the
//! destination engine could apply. The `_for(db, …)` entry points judge text
//! bound for a known engine; the dialect-free ones judge it for every
//! modelled engine at once (a verdict that must hold wherever it runs).
//!
//! Split by concern; every public item is re-exported here so callers keep
//! importing from `crate::query::safety::*` unchanged.

mod batch_lead;
#[cfg(test)]
mod bypass_tests;
mod classify;
mod policy;
mod production;
mod read_only;
pub mod redis;
mod statements;
mod statistics;

pub use batch_lead::{BatchLeadViolation, tsql_batch_lead_violation};
pub use classify::{
    BlockedSpan, DangerousArtifact, SafetyClassification, StatementKindHint, changes_schema_for,
    classify_select_only, classify_select_only_for, find_dangerous_artifact,
    find_dangerous_artifact_for, is_single_statement_for, statement_kind_hint,
    statement_kind_hint_for,
};
pub use policy::{
    EffectiveSql, apply_connection_policy, apply_connection_policy_for,
    check_connection_policy_for, preview_effective_sql, wrap_in_rollback_transaction,
    wrap_in_rollback_transaction_for,
};
pub use production::{ProductionDecision, ProductionGuard, UnlockWindow, WriteUnlock};
pub use read_only::{
    READ_ONLY_REFUSAL_PREFIX, ReadOnlyViolation, explain_read_only_refusal, is_read_only_refusal,
    read_only_violation, read_only_wrap_for,
};

pub use statistics::should_skip_statistics_wrap;
/// The clock the production/unlock layer measures against. `std::time::Instant`
/// on native; `web_time::Instant` in the browser (wasm), where the std clock
/// panics. Callers threading a `now` into these APIs should use this alias so
/// the type lines up on every target.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;
