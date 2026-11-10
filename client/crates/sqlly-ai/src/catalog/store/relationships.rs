//! Relationship edges: declared foreign keys, name-inferred references, and
//! neighbor-edge queries for graph expansion.

use std::collections::HashMap;

use rusqlite::{Connection, ToSql, params, params_from_iter};

use super::sql_util::{chunked_slices, chunked_slices_with_size, in_placeholders};

use crate::catalog::wire::CatalogObjectKind;

use super::{CatalogError, CatalogStore};

/// A pending inferred edge staged for bulk insert:
/// (from_object_key, from_column, to_object_key, to_namespace, to_name,
/// to_column, confidence, relationship_type).
type PendingEdge = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    f64,
    String,
);

/// A relationship row read back from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRelationship {
    pub from_object_key: String,
    pub from_column: String,
    pub to_object_key: Option<String>,
    pub to_namespace: Option<String>,
    pub to_name: String,
    pub to_column: Option<String>,
    /// `true` when this edge was inferred (e.g. from column naming) rather than
    /// declared as a real foreign key. Declared FKs are authoritative.
    pub is_inferred: bool,
    /// Confidence in `[0.0, 1.0]`. Declared FKs are always `1.0`.
    pub confidence: f64,
    /// Stable relationship-type token (`ForeignKey`, `NameInferredReference`, …).
    pub relationship_type: String,
}

/// An edge incident to one of the queried object keys, with direction.
#[derive(Debug, Clone, PartialEq)]
pub struct NeighborEdge {
    pub from_object_key: String,
    pub from_column: String,
    pub to_object_key: Option<String>,
    pub to_namespace: Option<String>,
    pub to_name: String,
    pub to_column: Option<String>,
    pub direction: NeighborDirection,
    /// `true` when this edge was inferred rather than a declared foreign key.
    pub is_inferred: bool,
    /// Confidence in `[0.0, 1.0]`. Declared FKs are always `1.0`.
    pub confidence: f64,
    /// Stable relationship-type token (`ForeignKey`, `NameInferredReference`, …).
    pub relationship_type: String,
}

/// Whether the edge is outbound from or inbound to the queried key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeighborDirection {
    Outbound,
    Inbound,
}

impl NeighborEdge {
    pub fn other_side(&self) -> Option<String> {
        match self.direction {
            NeighborDirection::Outbound => {
                self.to_object_key
                    .clone()
                    .or_else(|| match (&self.to_namespace, &self.to_name) {
                        (Some(ns), name) => Some(format!(
                            "{}.{}",
                            ns.to_ascii_lowercase(),
                            name.to_ascii_lowercase()
                        )),
                        (None, name) => Some(name.to_ascii_lowercase()),
                    })
            }
            NeighborDirection::Inbound => Some(self.from_object_key.clone()),
        }
    }
}

impl CatalogStore {
    pub fn relationships_from(
        &self,
        object_key: &str,
    ) -> Result<Vec<StoredRelationship>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
                    is_inferred, confidence, relationship_type
             FROM catalog_relationship WHERE from_object_key=?1",
        )?;
        let rows = stmt.query_map(params![object_key], stored_relationship_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn relationships_to(
        &self,
        object_key: &str,
    ) -> Result<Vec<StoredRelationship>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
                    is_inferred, confidence, relationship_type
             FROM catalog_relationship WHERE to_object_key=?1",
        )?;
        let rows = stmt.query_map(params![object_key], stored_relationship_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn neighbor_edges(
        &self,
        object_keys: &[String],
    ) -> Result<Vec<NeighborEdge>, CatalogError> {
        if object_keys.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let mut out = Vec::new();
        for chunk in chunked_slices(object_keys) {
            let n = chunk.len();
            let ph1 = in_placeholders(n, 1);
            let ph2 = in_placeholders(n, n + 1);
            let ph3 = in_placeholders(n, n * 2 + 1);

            let sql_from = format!(
                "SELECT from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
                        is_inferred, confidence, relationship_type, 0 AS direction
                 FROM catalog_relationship WHERE from_object_key IN ({ph1})"
            );

            let sql_to = format!(
                "SELECT from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
                        is_inferred, confidence, relationship_type, 1 AS direction
                 FROM catalog_relationship WHERE to_object_key IN ({ph2})
                 UNION ALL
                 SELECT r.from_object_key, r.from_column, r.to_object_key, r.to_namespace, r.to_name, r.to_column,
                        r.is_inferred, r.confidence, r.relationship_type, 1 AS direction
                 FROM catalog_relationship r
                 JOIN catalog_object o ON o.namespace IS r.to_namespace AND o.name = r.to_name AND o.is_deleted = 0
                 WHERE r.to_object_key IS NULL AND o.object_key IN ({ph3})"
            );

            let sql = format!("{sql_from} UNION ALL {sql_to}");
            let mut stmt = conn.prepare(&sql)?;

            let mut all_params: Vec<Box<dyn ToSql>> = Vec::new();
            for k in chunk {
                all_params.push(Box::new(k.clone()));
            }
            for k in chunk {
                all_params.push(Box::new(k.clone()));
            }
            for k in chunk {
                all_params.push(Box::new(k.clone()));
            }

            let param_refs: Vec<&dyn ToSql> = all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_from_iter(param_refs), neighbor_edge_from_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }
}

fn stored_relationship_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRelationship> {
    Ok(StoredRelationship {
        from_object_key: r.get(0)?,
        from_column: r.get(1)?,
        to_object_key: r.get(2)?,
        to_namespace: r.get(3)?,
        to_name: r.get(4)?,
        to_column: r.get(5)?,
        is_inferred: r.get::<_, i64>(6)? != 0,
        confidence: r.get(7)?,
        relationship_type: r.get(8)?,
    })
}

fn neighbor_edge_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NeighborEdge> {
    let dir_val: i64 = r.get(9)?;
    Ok(NeighborEdge {
        from_object_key: r.get(0)?,
        from_column: r.get(1)?,
        to_object_key: r.get(2)?,
        to_namespace: r.get(3)?,
        to_name: r.get(4)?,
        to_column: r.get(5)?,
        is_inferred: r.get::<_, i64>(6)? != 0,
        confidence: r.get(7)?,
        relationship_type: r.get(8)?,
        direction: if dir_val == 0 {
            NeighborDirection::Outbound
        } else {
            NeighborDirection::Inbound
        },
    })
}

/// Rebuild every `is_inferred=1` edge from the current live catalog state.
///
/// Declared foreign keys (`is_inferred=0`) are never touched. We wipe all
/// inferred rows, build a candidate-target index once from the live table/view
/// objects, then persist conservative name-inferred edges per source object.
/// Returns the number of inferred edges written.
pub(super) fn rebuild_inferred_relationships(tx: &Connection) -> Result<usize, CatalogError> {
    use crate::catalog::infer::{
        CandidateTargetIndex, InferenceColumn, InferenceObject, InferenceOptions,
        infer_relationships,
    };

    // Clear previously-inferred edges; declared FKs stay authoritative.
    tx.execute("DELETE FROM catalog_relationship WHERE is_inferred=1", [])?;

    // Load live objects (key, namespace, name, kind).
    struct Row {
        key: String,
        namespace: Option<String>,
        name: String,
        is_table_like: bool,
    }
    let mut rows: Vec<Row> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT object_key, namespace, name, kind
             FROM catalog_object WHERE is_deleted=0",
        )?;
        let mapped = stmt.query_map([], |r| {
            let kind: String = r.get(3)?;
            let is_table_like = matches!(
                CatalogObjectKind::parse(&kind),
                Some(CatalogObjectKind::Table) | Some(CatalogObjectKind::View)
            );
            Ok(Row {
                key: r.get(0)?,
                namespace: r.get(1)?,
                name: r.get(2)?,
                is_table_like,
            })
        })?;
        for r in mapped {
            rows.push(r?);
        }
    }

    if rows.is_empty() {
        return Ok(0);
    }

    // Load all columns grouped by object_key.
    let mut cols_by_key: HashMap<String, Vec<InferenceColumn>> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT object_key, name, type_name FROM catalog_column ORDER BY object_key, ordinal",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                InferenceColumn {
                    name: r.get(1)?,
                    type_name: r.get(2)?,
                },
            ))
        })?;
        for row in mapped {
            let (key, col) = row?;
            cols_by_key.entry(key).or_default().push(col);
        }
    }

    // Load declared-FK source columns per object so we never shadow a real key.
    let mut declared_by_key: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT from_object_key, from_column FROM catalog_relationship WHERE is_inferred=0",
        )?;
        let mapped =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in mapped {
            let (key, col) = row?;
            declared_by_key.entry(key).or_default().push(col);
        }
    }

    // Assemble inference objects.
    let objects: Vec<InferenceObject> = rows
        .iter()
        .map(|r| InferenceObject {
            object_key: r.key.clone(),
            namespace: r.namespace.clone(),
            name: r.name.clone(),
            is_table_like: r.is_table_like,
            columns: cols_by_key.get(&r.key).cloned().unwrap_or_default(),
        })
        .collect();

    let index = CandidateTargetIndex::build(&objects);
    let options = InferenceOptions::default();
    let empty: Vec<String> = Vec::new();

    let mut written = 0usize;
    let mut pending: Vec<PendingEdge> = Vec::new();
    for source in &objects {
        let declared = declared_by_key.get(&source.object_key).unwrap_or(&empty);
        let edges = infer_relationships(source, &index, declared, &options);
        for edge in edges {
            pending.push((
                edge.from_object_key,
                edge.from_column,
                edge.to_object_key,
                edge.to_namespace,
                edge.to_name,
                edge.to_column,
                edge.confidence,
                edge.relationship_type,
            ));
        }
    }

    const EDGE_INSERT_CHUNK: usize = 100;
    for chunk in chunked_slices_with_size(&pending, EDGE_INSERT_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let mut sql = String::from(
            "INSERT INTO catalog_relationship
                (from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
                 is_inferred, confidence, relationship_type) VALUES ",
        );
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();
        let mut idx = 1;
        for (i, row) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "(?{},?{},?{},?{},?{},?{},1,?{},?{})",
                idx,
                idx + 1,
                idx + 2,
                idx + 3,
                idx + 4,
                idx + 5,
                idx + 6,
                idx + 7
            ));
            idx += 8;
            params.push(Box::new(row.0.clone()));
            params.push(Box::new(row.1.clone()));
            params.push(Box::new(row.2.clone()));
            params.push(Box::new(row.3.clone()));
            params.push(Box::new(row.4.clone()));
            params.push(Box::new(row.5.clone()));
            params.push(Box::new(row.6));
            params.push(Box::new(row.7.clone()));
        }
        let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
        tx.execute(&sql, params_from_iter(param_refs))?;
        written += chunk.len();
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;
    use crate::catalog::wire::{CatalogForeignKey, CatalogObject};
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    #[test]
    fn sync_noop_skips_inferred_rebuild() {
        let store = CatalogStore::open_in_memory().unwrap();
        let orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        let report1 = store.sync(&[users.clone(), orders.clone()]).unwrap();
        assert_eq!(report1.inferred_relationships, 1);

        let report2 = store.sync(&[users, orders]).unwrap();
        assert!(report2.is_clean());
        assert_eq!(report2.inferred_relationships, 0);
        assert_eq!(store.relationships_from("dbo.orders").unwrap().len(), 1);
    }

    #[test]
    fn relationships_from_returns_outbound_edges() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();
        let rels = store.relationships_from("dbo.orders").unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].from_column, "user_id");
        assert_eq!(rels[0].to_object_key.as_deref(), Some("dbo.users"));
        assert_eq!(rels[0].to_name, "users");
    }

    #[test]
    fn relationships_to_returns_inbound_edges() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();
        let rels = store.relationships_to("dbo.users").unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].from_object_key, "dbo.orders");
        assert_eq!(rels[0].from_column, "user_id");
    }

    #[test]
    fn neighbor_edges_explain_uses_live_key_index_not_lower_scan() {
        let store = CatalogStore::open_in_memory().unwrap();
        let orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();

        let conn = store.conn.lock();
        let sql = "SELECT r.from_object_key
                   FROM catalog_relationship r
                   JOIN catalog_object o
                     ON o.namespace IS r.to_namespace AND o.name = r.to_name AND o.is_deleted = 0
                   WHERE r.to_object_key IS NULL AND o.object_key IN ('dbo.users')";
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
        let plan: Vec<String> = rows.map(|r| r.unwrap()).collect();
        assert!(
            plan.iter().any(|line| {
                line.contains("ix_catalog_object_live_key")
                    || line.contains("sqlite_autoindex_catalog_object_1")
            }),
            "expected indexed object_key lookup, got: {plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("lower(")),
            "plan should not apply lower() to indexed columns: {plan:?}"
        );
    }

    #[test]
    fn neighbor_edges_returns_both_directions() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();
        let edges = store.neighbor_edges(&["dbo.users".to_string()]).unwrap();
        assert!(!edges.is_empty(), "should find at least one edge for users");
        let has_inbound = edges.iter().any(|e| {
            e.direction == NeighborDirection::Inbound && e.from_object_key == "dbo.orders"
        });
        assert!(
            has_inbound,
            "should find inbound edge from orders, got {:?}",
            edges
        );
    }

    #[test]
    fn sync_infers_relationship_when_no_declared_fk() {
        let store = CatalogStore::open_in_memory().unwrap();
        // No foreign keys declared; `user_id` should infer a reference to `users`.
        let orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        let report = store.sync(&[users, orders]).unwrap();

        assert_eq!(report.inferred_relationships, 1);

        let rels = store.relationships_from("dbo.orders").unwrap();
        assert_eq!(rels.len(), 1, "got {:?}", rels);
        let edge = &rels[0];
        assert!(edge.is_inferred);
        assert_eq!(edge.from_column, "user_id");
        assert_eq!(edge.to_object_key.as_deref(), Some("dbo.users"));
        assert_eq!(edge.to_column.as_deref(), Some("id"));
        assert_eq!(
            edge.relationship_type,
            crate::catalog::infer::NAME_INFERRED_REFERENCE
        );
        assert!(
            edge.confidence >= 0.55 && edge.confidence <= 1.0,
            "confidence {} out of range",
            edge.confidence
        );
        // Inferred edges are surfaced through the graph choke point too.
        let edges = store.neighbor_edges(&["dbo.users".to_string()]).unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.is_inferred && e.from_object_key == "dbo.orders"),
            "neighbor_edges should expose inferred inbound edge, got {:?}",
            edges
        );
    }

    #[test]
    fn declared_fk_is_not_duplicated_by_inference() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        let report = store.sync(&[users, orders]).unwrap();

        // The one declared FK suppresses inference on the same column.
        assert_eq!(report.inferred_relationships, 0);
        let rels = store.relationships_from("dbo.orders").unwrap();
        assert_eq!(rels.len(), 1, "got {:?}", rels);
        assert!(!rels[0].is_inferred);
        assert_eq!(rels[0].confidence, 1.0);
        assert_eq!(rels[0].relationship_type, "ForeignKey");
    }

    #[test]
    fn declared_relationships_get_default_metadata() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();
        let rels = store.relationships_from("dbo.orders").unwrap();
        assert_eq!(rels.len(), 1);
        assert!(!rels[0].is_inferred);
        assert_eq!(rels[0].confidence, 1.0);
        assert_eq!(rels[0].relationship_type, "ForeignKey");
    }

    #[test]
    fn inferred_relationships_recomputed_on_resync() {
        let store = CatalogStore::open_in_memory().unwrap();
        // First sync without the target table: nothing to infer against.
        let orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(2)));
        let report1 = store.sync(std::slice::from_ref(&orders)).unwrap();
        assert_eq!(report1.inferred_relationships, 0);

        // Re-sync with the target table present: inference now succeeds.
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        let report2 = store.sync(&[users, orders]).unwrap();
        assert_eq!(report2.inferred_relationships, 1);
    }

    #[test]
    fn inference_scales_to_a_large_catalog() {
        // Smoke test that sync-time inference stays correct and bounded on a
        // catalog with many tables. Each `child_<i>` references the shared
        // `users` table via an inferred `user_id`, plus one extra distractor
        // column that must NOT infer anything.
        let store = CatalogStore::open_in_memory().unwrap();
        const N: usize = 400;

        let mut objects: Vec<CatalogObject> =
            vec![table("users", &["id"], db_source(Some(1), Some(2)))];
        for i in 0..N {
            let name = format!("child_{i}");
            // `user_id` infers -> users.id; `note` is a non-reference column.
            objects.push(table(
                &name,
                &["id", "user_id", "note"],
                db_source(Some(1), Some(2)),
            ));
        }

        let start = Instant::now();
        let report = store.sync(&objects).unwrap();
        let elapsed = start.elapsed();

        // Exactly one inferred edge per child table; the distractor never infers.
        assert_eq!(report.inferred_relationships, N);

        // Spot-check a few children resolved to the shared target.
        for i in [0usize, N / 2, N - 1] {
            let rels = store.relationships_from(&format!("dbo.child_{i}")).unwrap();
            assert_eq!(rels.len(), 1, "child_{i} edges: {rels:?}");
            assert!(rels[0].is_inferred);
            assert_eq!(rels[0].to_object_key.as_deref(), Some("dbo.users"));
        }

        // Generous ceiling: this is a regression guard against accidental
        // O(n^2) blow-ups in the rebuild path, not a micro-benchmark.
        assert!(
            elapsed.as_secs() < 10,
            "large-catalog sync took too long: {elapsed:?}"
        );
    }
}
