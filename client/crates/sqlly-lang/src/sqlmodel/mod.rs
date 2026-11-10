//! # sqlly-sqlmodel
//!
//! Canonical multi-dialect SQL data model (ADR-011). One model covers the
//! catalog concepts of SQL Server, PostgreSQL, MySQL, Oracle, and SQLite via
//! a normalized Catalog → Namespace → Object hierarchy plus per-dialect
//! capability flags. No I/O and no async: this crate only defines data and
//! pure operations over it (snapshots, deltas, name/symbol indexes).
//!
//! Module layout (everything is re-exported at the crate root, so downstream
//! code imports `crate::sqlmodel::X` regardless of which module defines `X`):
//! - [`dialect`] — dialects, identifier folding, names/references, object
//!   kinds, capability flags
//! - [`schema`] — object bodies, provenance/fidelity, entries, fragments,
//!   sources
//! - [`snapshot`] — [`ModelSnapshot`], [`ModelDelta`], conflict resolution,
//!   persistence format
//! - [`db_type`] / [`types`] — database-type metadata and the type system

#![forbid(unsafe_code)]

pub mod db_type;
pub mod dialect;
pub mod identifier_quoting;
pub mod schema;
pub mod snapshot;
pub mod transfer_types;
pub mod types;

pub use db_type::{
    ConnectionKind, DatabaseType, DbFeature, DbTypeMetadata, IdentifierQuoting, LogicalType,
    RowLimitSyntax, SqlKeyword, SqlOperation, TypeMapping, TypeStorage, UpsertPattern,
};
pub use dialect::{
    CatalogName, Dialect, DialectCapabilities, IdentifierFold, NamespaceName, ObjectKind,
    ObjectName, ObjectRef, SqlName, fold_identifier,
};
pub use identifier_quoting::is_reserved_word;
pub use schema::{
    CheckDef, ColumnDef, ContentHash, Fidelity, ForeignKeyDef, Generation, IncompleteRegions,
    KeyDef, ObjectBody, ObjectEntry, ObjectFragment, ObjectId, ParamDef, ParamMode, ProfileId,
    Provenance, RecoveryNote, RoutineBody, SourceDescriptor, SourceKind, Span, TableBody, ViewBody,
};
pub use snapshot::{ModelDelta, ModelSnapshot, SourceConflict};
pub use transfer_types::{MappedColumnType, MappingNote, logical_type_of, map_column_type};
pub use types::{TypeClass, TypeParams, TypeRef};
