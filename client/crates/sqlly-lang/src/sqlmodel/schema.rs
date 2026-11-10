//! Object schema definitions: bodies (tables, views, routines), provenance
//! and fidelity tracking, resolved entries ([`ObjectEntry`]), collector
//! fragments ([`ObjectFragment`]), source descriptors, and incomplete-region
//! bookkeeping.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::sqlmodel::dialect::{Dialect, ObjectKind, ObjectRef, SqlName};
use crate::sqlmodel::types::TypeRef;

// ---------------------------------------------------------------------------
// Object bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: SqlName,
    pub ty: Option<TypeRef>,
    pub nullable: Option<bool>,
    pub default: Option<String>,
    pub identity: bool,
    pub generated: Option<String>,
    pub collation: Option<String>,
    /// Ordinal position in the table (0-based).
    pub ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyDef {
    pub name: Option<SqlName>,
    pub columns: Vec<SqlName>,
    pub target: ObjectRef,
    pub target_columns: Vec<SqlName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyDef {
    pub name: Option<SqlName>,
    pub columns: Vec<SqlName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDef {
    pub name: Option<SqlName>,
    pub expression: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableBody {
    pub columns: Vec<ColumnDef>,
    pub primary_key: Option<KeyDef>,
    pub unique_keys: Vec<KeyDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub checks: Vec<CheckDef>,
    /// Dialect-specific table options (engine=InnoDB, fillfactor, …).
    pub options: BTreeMap<String, String>,
}

impl TableBody {
    pub fn column(&self, name: &SqlName) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name.matches(name))
    }

    pub fn column_by_fold(&self, fold: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name.fold_key() == fold)
    }

    /// Foreign key whose first (or only) column is `column`.
    pub fn foreign_key_for_column(&self, column: &SqlName) -> Option<&ForeignKeyDef> {
        self.foreign_keys
            .iter()
            .find(|fk| fk.columns.iter().any(|c| c.matches(column)))
    }

    pub fn is_key_column(&self, column: &SqlName) -> bool {
        self.primary_key
            .as_ref()
            .map(|k| k.columns.iter().any(|c| c.matches(column)))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamDef {
    pub name: SqlName,
    pub ty: Option<TypeRef>,
    pub mode: ParamMode,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineBody {
    pub params: Vec<ParamDef>,
    pub returns: Option<TypeRef>,
    pub language: Option<String>,
    /// Raw body text span within the defining source, where known.
    pub body_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewBody {
    pub columns: Vec<ColumnDef>,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectBody {
    Table(TableBody),
    View(ViewBody),
    Routine(RoutineBody),
    Synonym {
        target: ObjectRef,
    },
    /// Kinds we model only by name/kind for now (sequences, triggers, …).
    Opaque,
}

// ---------------------------------------------------------------------------
// Provenance / fidelity
// ---------------------------------------------------------------------------

pub type ProfileId = String;
pub type ContentHash = u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.start && offset <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Database {
        profile: ProfileId,
    },
    File {
        path: String,
        content_hash: ContentHash,
        span: Span,
    },
}

impl Provenance {
    pub fn is_file(&self) -> bool {
        matches!(self, Provenance::File { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryNote {
    pub kind: String,
    pub span: Span,
    pub assumption: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fidelity {
    Exact,
    Recovered { actions: Vec<RecoveryNote> },
}

impl Fidelity {
    pub fn is_exact(&self) -> bool {
        matches!(self, Fidelity::Exact)
    }
}

// ---------------------------------------------------------------------------
// Object entries
// ---------------------------------------------------------------------------

pub type ObjectId = u64;
pub type Generation = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    pub id: ObjectId,
    pub reference: ObjectRef,
    pub kind: ObjectKind,
    pub dialect: Dialect,
    pub body: ObjectBody,
    pub provenance: Provenance,
    /// Losing definition when sources conflict (file wins, database shadows).
    pub shadow: Option<Box<ObjectEntry>>,
    pub fidelity: Fidelity,
}

impl ObjectEntry {
    pub fn table(&self) -> Option<&TableBody> {
        match &self.body {
            ObjectBody::Table(t) => Some(t),
            _ => None,
        }
    }

    pub fn routine(&self) -> Option<&RoutineBody> {
        match &self.body {
            ObjectBody::Routine(r) => Some(r),
            _ => None,
        }
    }
}

/// A partial model piece emitted by a collector, before merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectFragment {
    pub reference: ObjectRef,
    pub kind: ObjectKind,
    pub dialect: Dialect,
    pub body: ObjectBody,
    pub provenance: Provenance,
    pub fidelity: Fidelity,
    /// Object references this definition mentions (FK targets, view sources…)
    /// used to build the symbol index.
    pub references: Vec<ObjectRef>,
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub id: String,
    pub dialect: Dialect,
    pub kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Database { profile: ProfileId },
    Filesystem { root: String },
}

/// Regions whose collection failed; model-scope validation reports `Unknown`
/// rather than `Missing` for references into these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteRegions {
    /// Fold-key prefixes ("catalog.namespace." or broader) that failed.
    pub prefixes: BTreeSet<String>,
}

impl IncompleteRegions {
    pub fn covers(&self, reference: &ObjectRef) -> bool {
        let key = reference.fold_key();
        self.prefixes.iter().any(|p| key.starts_with(p.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlmodel::dialect::Dialect;

    fn name(s: &str, d: Dialect) -> SqlName {
        SqlName::new(s, d, false)
    }

    #[test]
    fn incomplete_region_coverage() {
        let d = Dialect::SqlServer;
        let mut inc = IncompleteRegions::default();
        inc.prefixes.insert(".dbo.".into());
        let r = ObjectRef::new(None, Some(name("dbo", d)), name("Anything", d));
        assert!(inc.covers(&r));
        let other = ObjectRef::new(None, Some(name("sales", d)), name("Anything", d));
        assert!(!inc.covers(&other));
    }
}
