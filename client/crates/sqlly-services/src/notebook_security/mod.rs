//! Notebook security: what may run, share or be kept, decided in trusted
//! Rust code at the moment it happens (spec 5.2, 5.3, 9.2, 11.2, 12).
//!
//! - [`trust`]: per-notebook trust levels keyed by notebook id and a hash of
//!   the executable parts, the grants that authorize connections, and the
//!   local trust store. Opening a notebook never runs anything.
//! - [`authorize`]: one check per action (run a cell, export, materialize,
//!   share with AI, schedule, publish) returning allow, deny or needs
//!   approval with a reason key.
//! - [`scope`]: cache scopes derived from the effective identity, so cached
//!   results never cross principals.
//! - [`cache`] (native): the encrypted snapshot store under unguessable
//!   scope directories, with its key in the OS credential store.
//! - [`redact`]: export redaction and secret detection.
//! - [`scan`]: malicious-file defenses for `.sqllynb` / `.sqlnb.json` /
//!   `.ipynb` text before it is loaded.
//! - [`audit`]: audit events, the local JSON-lines log (native) and the
//!   mapping onto the Embedded query audit contract.
//! - [`prompt`]: fencing and sanitizing untrusted text before it reaches a
//!   model.
//!
//! Everything except [`cache`] and the file-backed stores is pure and
//! compiles for wasm. Reasons and findings carry `notebook.security.*`
//! message keys with English fallbacks.

pub mod audit;
pub mod authorize;
#[cfg(not(target_arch = "wasm32"))]
pub mod cache;
mod messages;
pub mod prompt;
pub mod redact;
pub mod scan;
pub mod scope;
pub mod trust;
#[cfg(not(target_arch = "wasm32"))]
pub mod workspace_keys;

#[cfg(test)]
mod adversarial_tests;

pub use audit::{AuditAction, AuditDecision, AuditEvent, AuditPrivacy};
pub use authorize::{
    AuthContext, ConnectionFacts, Consents, DataPolicy, Decision, NotebookAction, Notice, Reason,
    authorize,
};
#[cfg(not(target_arch = "wasm32"))]
pub use cache::{CacheKeyProvider, KeyringCacheKey, MemoryCacheKey, SnapshotStore};
pub use prompt::{UntrustedSource, fence_untrusted, sanitize_result_summary};
pub use redact::{ExportPolicy, ExportWarning, RedactedExport, find_secrets, redact_for_export};
pub use scan::{Finding, ScanLimits, ScanReport, scan_notebook_file};
pub use scope::CacheScope;
pub use trust::{
    Grant, TrustDecision, TrustLevel, TrustReason, TrustRecord, TrustStore, executable_hash,
};
