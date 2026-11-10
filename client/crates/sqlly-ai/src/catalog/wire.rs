//! Wire types describing schema objects handed to the catalog for persistence.
//!
//! These mirror the conventions of the CLI's `SchemaObject` payload but carry
//! the extra information the catalog needs: a source descriptor (so we can tell
//! database objects from filesystem DDL) and create/modify timestamps (so change
//! detection can short-circuit without recomputing a hash).
//!
//! Everything here is plain `Serialize`/`Deserialize` so the engine subprocess
//! can decode it straight out of its JSON request frames.

use serde::{Deserialize, Serialize};

/// One schema object to be cataloged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogObject {
    /// Containing schema/namespace (e.g. `dbo`). `None` for dialects without
    /// namespaces or for catalog-level objects.
    pub schema: Option<String>,
    /// Unquoted object name, e.g. `Customers`.
    pub name: String,
    /// What kind of object this is.
    #[serde(rename = "type")]
    pub kind: CatalogObjectKind,
    /// Columns for tables/views, parameters for routines. Empty otherwise.
    pub columns: Vec<CatalogColumn>,
    /// Primary-key column names for a table, in key order. Empty when the object
    /// has no primary key or the source never reported one. `#[serde(default)]`
    /// so catalog blobs persisted before this field existed still deserialize.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Foreign-key relationships declared by this object (tables/views).
    pub foreign_keys: Vec<CatalogForeignKey>,
    /// Routine parameters (procedures/functions). Empty otherwise.
    pub parameters: Vec<CatalogParameter>,
    /// Optional definition text (view SQL, routine body, etc.).
    pub definition: Option<String>,
    /// Where this object came from + its freshness signal.
    pub source: CatalogSource,
}

/// Coarse object classification. Kept deliberately small; the catalog stores the
/// kind as a stable string so adding a variant never invalidates existing rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogObjectKind {
    Table,
    View,
    StoredProcedure,
    Function,
    Type,
}

impl CatalogObjectKind {
    /// Stable lowercase token persisted in SQLite and used in hash input.
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogObjectKind::Table => "table",
            CatalogObjectKind::View => "view",
            CatalogObjectKind::StoredProcedure => "procedure",
            CatalogObjectKind::Function => "function",
            CatalogObjectKind::Type => "type",
        }
    }
}

impl CatalogObjectKind {
    /// Parse a kind token, accepting both the serde form and common aliases
    /// used elsewhere in the codebase (`sproc`, `scalar_function`, ...).
    pub fn parse(token: &str) -> Option<CatalogObjectKind> {
        match token.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "view" => Some(Self::View),
            "procedure" | "sproc" | "stored_procedure" | "storedProcedure" => {
                Some(Self::StoredProcedure)
            }
            "function" | "scalar_function" | "table_function" => Some(Self::Function),
            "type" => Some(Self::Type),
            _ => None,
        }
    }
}

/// A single column (for tables/views) or parameter (for routines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogColumn {
    /// Column name, unquoted.
    pub name: String,
    /// Declared type if known (e.g. `INT`, `NVARCHAR(50)`).
    #[serde(default)]
    pub type_name: Option<String>,
    /// 1-based ordinal within the parent.
    #[serde(default)]
    pub ordinal: i64,
    /// `true` when the column admits NULL.
    #[serde(default)]
    pub nullable: bool,
}

/// A foreign-key relationship declared by a table/view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogForeignKey {
    /// Local columns participating in the key, in declared order.
    pub columns: Vec<String>,
    /// Referenced schema, if known.
    pub target_schema: Option<String>,
    /// Referenced table name.
    pub target_table: String,
    /// Referenced columns, parallel to `columns`.
    pub target_columns: Vec<String>,
}

/// A routine parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogParameter {
    pub name: String,
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub is_output: bool,
}

/// Provenance + freshness signal for an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSource {
    #[serde(rename = "type")]
    pub kind: SourceKind,
    /// File path (for `File`) or connection profile id (for `Database`).
    #[serde(default)]
    pub path: Option<String>,
    /// Wall-clock create time, unix milliseconds, if the engine exposes one.
    #[serde(default)]
    pub create_date: Option<i64>,
    /// Wall-clock modify time, unix milliseconds, if the engine exposes one.
    #[serde(default)]
    pub modify_date: Option<i64>,
}

impl CatalogSource {
    /// The freshness timestamp used to gate hash recomputation.
    ///
    /// Per the design: take the newer of create/modify when both are present.
    /// For file-system DDL this is `max(mtime, ctime)`; for databases it is
    /// `max(create_date, modify_date)`. Returns `None` when the engine cannot
    /// report either timestamp — in that case change detection falls back to
    /// recomputing the hash on every sync (the hash is the source of truth).
    pub fn source_timestamp(&self) -> Option<i64> {
        match (self.create_date, self.modify_date) {
            (Some(c), Some(m)) => Some(c.max(m)),
            (Some(c), None) => Some(c),
            (None, Some(m)) => Some(m),
            (None, None) => None,
        }
    }
}

/// Whether an object was observed from a live database or from a DDL file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Database,
    File,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Database => "database",
            SourceKind::File => "file",
        }
    }
}

impl CatalogObject {
    /// Canonical, fold-independent key identifying this object within a scope.
    ///
    /// Mirrors `ObjectRef::fold_key`: `schema.name` (lower-cased), so the same
    /// object spelled with different casing collapses to one row.
    pub fn object_key(&self) -> String {
        match &self.schema {
            Some(schema) => format!(
                "{}.{}",
                schema.to_ascii_lowercase(),
                self.name.to_ascii_lowercase()
            ),
            None => self.name.to_ascii_lowercase(),
        }
    }

    /// Convenience: the source timestamp, if any.
    pub fn source_timestamp(&self) -> Option<i64> {
        self.source.source_timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(create: Option<i64>, modify: Option<i64>) -> CatalogSource {
        CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: create,
            modify_date: modify,
        }
    }

    #[test]
    fn source_timestamp_takes_max_of_create_and_modify() {
        assert_eq!(src(Some(100), Some(300)).source_timestamp(), Some(300));
        assert_eq!(src(Some(300), Some(100)).source_timestamp(), Some(300));
    }

    #[test]
    fn source_timestamp_falls_back_to_whichever_is_present() {
        assert_eq!(src(Some(100), None).source_timestamp(), Some(100));
        assert_eq!(src(None, Some(200)).source_timestamp(), Some(200));
    }

    #[test]
    fn source_timestamp_is_none_when_engine_reports_nothing() {
        assert_eq!(src(None, None).source_timestamp(), None);
    }

    #[test]
    fn object_key_is_case_insensitive_and_namespaced() {
        let obj = CatalogObject {
            schema: Some("dbo".into()),
            name: "Customers".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: src(None, None),
        };
        assert_eq!(obj.object_key(), "dbo.customers");
    }

    #[test]
    fn kind_parses_aliases() {
        assert_eq!(
            CatalogObjectKind::parse("sproc"),
            Some(CatalogObjectKind::StoredProcedure)
        );
        assert_eq!(
            CatalogObjectKind::parse("TABLE"),
            Some(CatalogObjectKind::Table)
        );
        assert_eq!(CatalogObjectKind::parse("nope"), None);
    }
}
