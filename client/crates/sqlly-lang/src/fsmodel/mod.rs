//! # sqlly-fsmodel
//!
//! Filesystem-sourced database model. Defines the portable, serializable data
//! model for SQL objects discovered from `.sql` files on disk: schemas, tables,
//! views, procedures, functions, types, and their columns/parameters/result
//! sets ([`NamedSqlNode`] and the typed wrappers around it). The model is the
//! shared vocabulary between the tokenizer ([`sqlly-tokenizer`]) that produces
//! it and the IntelliSense/symbol index ([`sqlly-intellisense`]) that consumes
//! it.
//!
//! The [`index`] submodule adds the filesystem *pipeline* primitives:
//! [`FilesystemDatabaseProfile`] (which roots/extensions to scan),
//! [`FileIndex`] (path→content-hash change detection), [`content_hash`],
//! [`flatten_snapshot`], [`compute_delta`], and [`merge_snapshots`]. The actual
//! directory walk + debounced watcher live in `sqlly-tokenizer::indexer` so the
//! data model stays dependency-light (serde only).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub mod index;

pub use index::{
    FileIndex, FilesystemDatabaseProfile, TokenizerMode, compute_delta, content_hash,
    flatten_snapshot, merge_snapshots, normalize_path_separators,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SqlNodeId(pub String);

impl SqlNodeId {
    pub fn new(kind: SqlNodeKind, schema: Option<&str>, parent: Option<&str>, name: &str) -> Self {
        let schema = schema.unwrap_or_default().to_ascii_lowercase();
        let parent = parent.unwrap_or_default().to_ascii_lowercase();
        Self(format!(
            "{:?}|{}|{}|{}",
            kind,
            schema,
            parent,
            name.to_ascii_lowercase()
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SqlNodeKind {
    Schema,
    Table,
    View,
    Procedure,
    Function,
    Type,
    Column,
    Parameter,
    ResultSet,
    ResultSetColumn,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SqlTypeLength {
    Max,
    Value(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SqlDataType {
    BuiltIn {
        name: String,
        length: Option<SqlTypeLength>,
        precision: Option<u8>,
        scale: Option<u8>,
    },
    Custom {
        schema: Option<String>,
        name: String,
    },
    Null,
    #[default]
    Unknown,
}

impl SqlDataType {
    pub fn built_in(name: impl Into<String>) -> Self {
        Self::BuiltIn {
            name: name.into(),
            length: None,
            precision: None,
            scale: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ParseConfidence {
    Exact,
    Inferred,
    Partial,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSourceRef {
    pub source_file_name: String,
    pub source_file_path: String,
    pub file_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedSqlNode {
    pub id: SqlNodeId,
    pub schema: Option<String>,
    pub parent: Option<SqlNodeId>,
    pub name: String,
    pub kind: SqlNodeKind,
    pub data_type: SqlDataType,
    pub confidence: ParseConfidence,
    pub span: Option<SourceSpan>,
}

impl NamedSqlNode {
    pub fn new(
        kind: SqlNodeKind,
        schema: Option<String>,
        parent: Option<SqlNodeId>,
        name: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let parent_key = parent.as_ref().map(|id| id.0.as_str());
        let id = SqlNodeId::new(kind, schema.as_deref(), parent_key, &name);
        Self {
            id,
            schema,
            parent,
            name,
            kind,
            data_type: SqlDataType::Unknown,
            confidence: ParseConfidence::Unknown,
            span: None,
        }
    }

    pub fn with_data_type(mut self, data_type: SqlDataType) -> Self {
        self.data_type = data_type;
        self
    }

    pub fn with_confidence(mut self, confidence: ParseConfidence) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSourcedSqlNode {
    pub node: NamedSqlNode,
    pub source: FileSourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlColumnNode {
    pub node: NamedSqlNode,
    pub nullable: Option<bool>,
    pub is_identity: bool,
    pub default: Option<String>,
    pub foreign_key: Option<String>,
    pub check_constraints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlParameterNode {
    pub node: NamedSqlNode,
    pub default: Option<String>,
    pub is_output: bool,
    pub is_readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlResultColumnNode {
    pub node: NamedSqlNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlResultSetNode {
    pub node: NamedSqlNode,
    pub columns: Vec<SqlResultColumnNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSqlNode {
    pub source_node: FileSourcedSqlNode,
    pub input_parameters: Vec<SqlParameterNode>,
    pub result_sets: Vec<SqlResultSetNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureSqlNode {
    pub operation: OperationSqlNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSqlNode {
    pub operation: OperationSqlNode,
    pub output: SqlDataType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSqlNode {
    pub source_node: FileSourcedSqlNode,
    pub columns: Vec<SqlColumnNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSqlNode {
    pub source_node: FileSourcedSqlNode,
    pub columns: Vec<SqlColumnNode>,
    pub select_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeSqlNode {
    pub source_node: FileSourcedSqlNode,
    pub underlying_type: SqlDataType,
    pub columns: Vec<SqlColumnNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSqlNode {
    pub node: NamedSqlNode,
    pub tables: Vec<TableSqlNode>,
    pub views: Vec<ViewSqlNode>,
    pub functions: Vec<FunctionSqlNode>,
    pub types: Vec<TypeSqlNode>,
    pub procedures: Vec<ProcedureSqlNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlModelSnapshot {
    pub schemas: Vec<SchemaSqlNode>,
    pub diagnostics: Vec<SqlModelDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlModelDiagnostic {
    pub message: String,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlModelDelta {
    pub added: Vec<NamedSqlNode>,
    pub updated: Vec<NamedSqlNode>,
    pub removed: Vec<SqlNodeId>,
    pub diagnostics: Vec<SqlModelDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_node_ids_are_deterministic() {
        let left = NamedSqlNode::new(SqlNodeKind::Table, Some("dbo".to_string()), None, "Orders");
        let right = NamedSqlNode::new(SqlNodeKind::Table, Some("dbo".to_string()), None, "orders");
        assert_eq!(left.id, right.id);
    }
}
