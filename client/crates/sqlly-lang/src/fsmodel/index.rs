//! Filesystem database profile + file index + delta computation.
//!
//! A [`FilesystemDatabaseProfile`] points the model at a set of root folders and
//! says how to tokenize them. The [`FileIndex`] tracks the content hash of every
//! indexed file so an incremental rescan can skip unchanged files and emit a
//! [`SqlModelDelta`](crate::fsmodel::SqlModelDelta) instead of a full snapshot.
//!
//! These are pure, dependency-free data structures so they can live in the model
//! crate; the actual tokenize-and-merge pipeline lives in `sqlly-tokenizer`
//! (which depends on this crate) to avoid a dependency cycle.

use crate::fsmodel::{NamedSqlNode, SchemaSqlNode, SqlModelDelta, SqlModelSnapshot, SqlNodeId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which tokenizer a source folder is parsed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TokenizerMode {
    /// Lightweight regex/scan tokenizer (the implemented one).
    #[default]
    Simple,
    /// Deep parse (reserved for a future tree-sitter backend; falls back to
    /// [`TokenizerMode::Simple`] until implemented).
    Full,
}

/// A user-configured filesystem-sourced database: friendly name, the roots to
/// scan, include/exclude globs, the tokenizer mode, and organizational metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemDatabaseProfile {
    pub name: String,
    pub root_paths: Vec<String>,
    /// Glob-ish suffix filters; a file is included if it ends with any of these
    /// (default `.sql`). Kept simple + allocation-light for a linear scan.
    pub include_extensions: Vec<String>,
    /// Path substrings to skip (e.g. `node_modules`, `bin/`, `obj/`).
    pub exclude_contains: Vec<String>,
    pub tokenizer_mode: TokenizerMode,
    pub client: Option<String>,
    pub project: Option<String>,
    pub environment: Option<String>,
}

impl Default for FilesystemDatabaseProfile {
    fn default() -> Self {
        Self {
            name: "Filesystem Database".to_string(),
            root_paths: Vec::new(),
            include_extensions: vec![".sql".to_string()],
            exclude_contains: vec![
                "/.git/".to_string(),
                "/node_modules/".to_string(),
                "/bin/".to_string(),
                "/obj/".to_string(),
                "/target/".to_string(),
            ],
            tokenizer_mode: TokenizerMode::Simple,
            client: None,
            project: None,
            environment: None,
        }
    }
}

impl FilesystemDatabaseProfile {
    pub fn new(name: impl Into<String>, root_paths: Vec<String>) -> Self {
        Self {
            name: name.into(),
            root_paths,
            ..Default::default()
        }
    }

    /// Whether a path should be indexed under this profile (extension match and
    /// not excluded). Path comparison is case-insensitive on the extension.
    pub fn accepts_path(&self, path: &str) -> bool {
        let lower = normalize_path_separators(path).to_ascii_lowercase();
        if self
            .exclude_contains
            .iter()
            .any(|x| lower.contains(&x.to_ascii_lowercase()))
        {
            return false;
        }
        if self.include_extensions.is_empty() {
            return true;
        }
        self.include_extensions
            .iter()
            .any(|ext| lower.ends_with(&ext.to_ascii_lowercase()))
    }
}

/// Normalize `\` to `/` so slash-delimited exclude patterns match Windows paths.
pub fn normalize_path_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// A content hash for a file. We use FNV-1a (64-bit) to avoid pulling in a hash
/// crate; it is stable and fast for the change-detection use case (not security).
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Records the hash of each indexed file so a rescan can skip unchanged files
/// and so deletes/renames can be detected by absence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIndex {
    /// path -> content hash.
    pub files: BTreeMap<String, String>,
}

impl FileIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if the file is new or its hash changed since last index.
    pub fn is_changed(&self, path: &str, hash: &str) -> bool {
        self.files.get(path).map(|h| h != hash).unwrap_or(true)
    }

    pub fn record(&mut self, path: impl Into<String>, hash: impl Into<String>) {
        self.files.insert(path.into(), hash.into());
    }

    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
    }

    pub fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
}

/// Flattens every node in a snapshot into a flat `id -> node` map for delta
/// computation. Includes schemas, tables/views/types/procs/functions and their
/// columns, parameters, result sets and result-set columns.
pub fn flatten_snapshot(snapshot: &SqlModelSnapshot) -> BTreeMap<SqlNodeId, NamedSqlNode> {
    let mut out: BTreeMap<SqlNodeId, NamedSqlNode> = BTreeMap::new();
    let mut push = |n: &NamedSqlNode| {
        out.insert(n.id.clone(), n.clone());
    };
    for schema in &snapshot.schemas {
        push(&schema.node);
        for t in &schema.tables {
            push(&t.source_node.node);
            for c in &t.columns {
                push(&c.node);
            }
        }
        for v in &schema.views {
            push(&v.source_node.node);
            for c in &v.columns {
                push(&c.node);
            }
        }
        for ty in &schema.types {
            push(&ty.source_node.node);
            for c in &ty.columns {
                push(&c.node);
            }
        }
        for p in &schema.procedures {
            push(&p.operation.source_node.node);
            for param in &p.operation.input_parameters {
                push(&param.node);
            }
            for rs in &p.operation.result_sets {
                push(&rs.node);
                for c in &rs.columns {
                    push(&c.node);
                }
            }
        }
        for f in &schema.functions {
            push(&f.operation.source_node.node);
            for param in &f.operation.input_parameters {
                push(&param.node);
            }
            for rs in &f.operation.result_sets {
                push(&rs.node);
                for c in &rs.columns {
                    push(&c.node);
                }
            }
        }
    }
    out
}

/// Computes the [`SqlModelDelta`] between a previous and a next snapshot by
/// flattening both and diffing node ids (added / removed) and node contents
/// (updated). Diagnostics from the next snapshot are carried through.
pub fn compute_delta(previous: &SqlModelSnapshot, next: &SqlModelSnapshot) -> SqlModelDelta {
    let prev = flatten_snapshot(previous);
    let now = flatten_snapshot(next);

    let mut delta = SqlModelDelta::default();
    for (id, node) in &now {
        match prev.get(id) {
            None => delta.added.push(node.clone()),
            Some(old) if old != node => delta.updated.push(node.clone()),
            Some(_) => {}
        }
    }
    for id in prev.keys() {
        if !now.contains_key(id) {
            delta.removed.push(id.clone());
        }
    }
    delta.diagnostics = next.diagnostics.clone();
    delta
}

/// Merges multiple per-file snapshots into one, combining schemas by name so
/// objects from many files land under a single schema node.
pub fn merge_snapshots(snapshots: Vec<SqlModelSnapshot>) -> SqlModelSnapshot {
    let mut by_schema: BTreeMap<String, SchemaSqlNode> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for snap in snapshots {
        diagnostics.extend(snap.diagnostics);
        for schema in snap.schemas {
            let key = schema.node.name.to_ascii_lowercase();
            match by_schema.get_mut(&key) {
                None => {
                    by_schema.insert(key, schema);
                }
                Some(existing) => {
                    existing.tables.extend(schema.tables);
                    existing.views.extend(schema.views);
                    existing.functions.extend(schema.functions);
                    existing.types.extend(schema.types);
                    existing.procedures.extend(schema.procedures);
                }
            }
        }
    }
    SqlModelSnapshot {
        schemas: by_schema.into_values().collect(),
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsmodel::{NamedSqlNode, SchemaSqlNode, SqlNodeKind};

    fn schema_snapshot(name: &str, table: &str) -> SqlModelSnapshot {
        use crate::fsmodel::{FileSourceRef, FileSourcedSqlNode, TableSqlNode};
        let schema_node = NamedSqlNode::new(SqlNodeKind::Schema, None, None, name);
        let table_node = NamedSqlNode::new(
            SqlNodeKind::Table,
            Some(name.to_string()),
            Some(schema_node.id.clone()),
            table,
        );
        SqlModelSnapshot {
            schemas: vec![SchemaSqlNode {
                node: schema_node,
                tables: vec![TableSqlNode {
                    source_node: FileSourcedSqlNode {
                        node: table_node,
                        source: FileSourceRef {
                            source_file_name: "f.sql".into(),
                            source_file_path: "/f.sql".into(),
                            file_hash: "h".into(),
                        },
                    },
                    columns: vec![],
                }],
                views: vec![],
                functions: vec![],
                types: vec![],
                procedures: vec![],
            }],
            diagnostics: vec![],
        }
    }

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        assert_eq!(
            content_hash(b"CREATE TABLE x"),
            content_hash(b"CREATE TABLE x")
        );
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
    }

    #[test]
    fn file_index_detects_changes() {
        let mut idx = FileIndex::new();
        assert!(idx.is_changed("/a.sql", "h1"));
        idx.record("/a.sql", "h1");
        assert!(!idx.is_changed("/a.sql", "h1"));
        assert!(idx.is_changed("/a.sql", "h2"));
        idx.remove("/a.sql");
        assert!(!idx.contains("/a.sql"));
    }

    #[test]
    fn profile_accepts_by_extension_and_excludes() {
        let p = FilesystemDatabaseProfile::new("db", vec!["/root".into()]);
        assert!(p.accepts_path("/root/dbo.Orders.sql"));
        assert!(!p.accepts_path("/root/readme.md"));
        assert!(!p.accepts_path("/root/node_modules/x.sql"));
    }

    #[test]
    fn profile_accepts_windows_paths_with_backslashes() {
        let p = FilesystemDatabaseProfile::default();
        assert!(p.accepts_path(r"c:\users\robin\project\Orders.sql"));
        assert!(p.accepts_path(r"c:\users\robin\src\Orders.sql"));
        assert!(!p.accepts_path(r"c:\repo\bin\foo.sql"));
    }

    #[test]
    fn merge_combines_schemas_by_name() {
        let merged = merge_snapshots(vec![
            schema_snapshot("dbo", "Orders"),
            schema_snapshot("dbo", "Customers"),
        ]);
        assert_eq!(merged.schemas.len(), 1);
        assert_eq!(merged.schemas[0].tables.len(), 2);
    }

    #[test]
    fn delta_reports_added_updated_removed() {
        let before = schema_snapshot("dbo", "Orders");
        let after = schema_snapshot("dbo", "Customers");
        let delta = compute_delta(&before, &after);
        // Customers table + (schema id is identical so not added) -> added has the new table.
        assert!(delta.added.iter().any(|n| n.name == "Customers"));
        assert!(
            delta
                .removed
                .iter()
                .any(|id| id.0.to_lowercase().contains("orders"))
        );
    }
}
