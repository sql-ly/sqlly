//! Reading live objects back out of the catalog: single-object fetch, bulk
//! loads for context assembly/search, and the slim SQL-validation index rows.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, ToSql, params, params_from_iter};

use crate::catalog::wire::{CatalogObject, CatalogObjectKind, SourceKind};

use super::sql_util::in_placeholders;
use super::{CatalogError, CatalogStore};

/// A row read back from the catalog.
#[derive(Debug, Clone)]
pub struct StoredObject {
    pub object_key: String,
    pub catalog: Option<String>,
    pub namespace: Option<String>,
    pub name: String,
    pub kind: CatalogObjectKind,
    pub source_kind: SourceKind,
    pub source_path: Option<String>,
    pub source_timestamp: Option<i64>,
    pub object_hash: String,
    pub body_json: String,
    pub ai_cache_valid: bool,
    pub ai_card_json: Option<String>,
}

impl StoredObject {
    /// Decode the stored body back into a wire object (without source/ts).
    pub fn to_catalog_object(&self) -> Result<CatalogObject, CatalogError> {
        let mut obj: CatalogObject = serde_json::from_str(&self.body_json)?;
        // Restore identity from the canonical columns so callers don't have to
        // rely on the body having serialized the schema/name.
        obj.schema = self.namespace.clone();
        obj.name = self.name.clone();
        obj.kind = self.kind;
        obj.source.kind = self.source_kind;
        obj.source.path = self.source_path.clone();
        obj.source.create_date = None;
        obj.source.modify_date = self.source_timestamp;
        Ok(obj)
    }
}

/// Object metadata used to build the SQL validation name index.
#[derive(Debug, Clone)]
pub struct ValidationIndexRow {
    pub object_key: String,
    pub catalog: Option<String>,
    pub namespace: Option<String>,
    pub name: String,
    pub kind: CatalogObjectKind,
    pub columns: Vec<String>,
    pub primary_key: Vec<String>,
}

impl CatalogStore {
    /// Count live (non-tombstoned) objects.
    pub fn live_object_count(&self) -> Result<usize, CatalogError> {
        Ok(self.conn.lock().query_row(
            "SELECT COUNT(*) FROM catalog_object WHERE is_deleted=0",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// Slim live-object rows for SQL validation: identity, column names, and
    /// primary keys without deserializing every `body_json`.
    pub fn live_validation_index_rows(&self) -> Result<Vec<ValidationIndexRow>, CatalogError> {
        let conn = self.conn.lock();
        let mut objects: Vec<ValidationIndexRow> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT object_key, catalog, namespace, name, kind,
                        json_extract(body_json, '$.primary_key') AS primary_key_json
                 FROM catalog_object WHERE is_deleted=0",
            )?;
            let rows = stmt.query_map([], |r| {
                let kind_str: String = r.get(4)?;
                let pk_json: Option<String> = r.get(5)?;
                Ok((
                    r.get::<_, String>(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, String>(3)?,
                    CatalogObjectKind::parse(&kind_str).unwrap_or(CatalogObjectKind::Table),
                    parse_primary_key_json(pk_json),
                ))
            })?;
            for row in rows {
                let (object_key, catalog, namespace, name, kind, primary_key) = row?;
                objects.push(ValidationIndexRow {
                    object_key,
                    catalog,
                    namespace,
                    name,
                    kind,
                    columns: Vec::new(),
                    primary_key,
                });
            }
        }

        let mut columns_by_key: HashMap<String, Vec<String>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT object_key, name FROM catalog_column ORDER BY object_key, ordinal",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (key, name) = row?;
                columns_by_key.entry(key).or_default().push(name);
            }
        }

        let mut legacy_keys: Vec<String> = Vec::new();
        for obj in &mut objects {
            if let Some(columns) = columns_by_key.remove(&obj.object_key) {
                obj.columns = columns;
            } else {
                legacy_keys.push(obj.object_key.clone());
            }
        }

        if !legacy_keys.is_empty() {
            for chunk in super::sql_util::chunked_slices(&legacy_keys) {
                let placeholders = in_placeholders(chunk.len(), 1);
                let sql = format!(
                    "SELECT object_key, body_json FROM catalog_object WHERE object_key IN ({})",
                    placeholders
                );
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn ToSql> = chunk.iter().map(|k| k as &dyn ToSql).collect();
                let rows = stmt.query_map(params_from_iter(param_refs), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                let mut bodies: HashMap<String, String> = HashMap::new();
                for row in rows {
                    let (key, body) = row?;
                    bodies.insert(key, body);
                }
                for obj in &mut objects {
                    if obj.columns.is_empty()
                        && let Some(body_json) = bodies.get(&obj.object_key)
                    {
                        let parsed: CatalogObject = serde_json::from_str(body_json)?;
                        obj.columns = parsed.columns.into_iter().map(|c| c.name).collect();
                        if obj.primary_key.is_empty() {
                            obj.primary_key = parsed.primary_key;
                        }
                    }
                }
            }
        }

        Ok(objects)
    }

    /// All live object keys — used by join-path discovery to avoid per-edge lookups.
    pub fn live_object_key_set(&self) -> Result<HashSet<String>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT object_key FROM catalog_object WHERE is_deleted=0")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    /// Load all live objects as wire objects (for context assembly / search).
    pub fn live_objects(&self) -> Result<Vec<StoredObject>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT object_key, catalog, namespace, name, kind, source_kind,
                    source_path, source_timestamp, object_hash, body_json,
                    ai_cache_valid, ai_card_json
             FROM catalog_object WHERE is_deleted=0",
        )?;
        let rows = stmt.query_map([], stored_object_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Load a deterministic, bounded slice of live objects for broad/noise-only
    /// questions and unhealthy-index fallback. This avoids full-catalog scans in
    /// the `ask_schema` path.
    pub fn live_objects_limited(&self, limit: usize) -> Result<Vec<StoredObject>, CatalogError> {
        let limit = limit.clamp(1, 100);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT object_key, catalog, namespace, name, kind, source_kind,
                    source_path, source_timestamp, object_hash, body_json,
                    ai_cache_valid, ai_card_json
             FROM catalog_object WHERE is_deleted=0
             ORDER BY name ASC, object_key ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], stored_object_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Fetch one stored object by key.
    pub fn get_object(&self, key: &str) -> Result<Option<StoredObject>, CatalogError> {
        self.conn
            .lock()
            .query_row(
                "SELECT object_key, catalog, namespace, name, kind, source_kind,
                        source_path, source_timestamp, object_hash, body_json,
                        ai_cache_valid, ai_card_json
                 FROM catalog_object WHERE object_key=?1 AND is_deleted=0",
                params![key],
                stored_object_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn objects_by_keys(&self, keys: &[String]) -> Result<Vec<StoredObject>, CatalogError> {
        let mut out = Vec::new();
        for key in keys {
            if let Some(object) = self.get_object(key)? {
                out.push(object);
            }
        }
        Ok(out)
    }
}

pub(super) fn stored_object_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObject> {
    let kind_str: String = r.get(4)?;
    let source_kind_str: String = r.get(5)?;
    Ok(StoredObject {
        object_key: r.get(0)?,
        catalog: r.get(1)?,
        namespace: r.get(2)?,
        name: r.get(3)?,
        kind: CatalogObjectKind::parse(&kind_str).unwrap_or(CatalogObjectKind::Table),
        source_kind: parse_source_kind(&source_kind_str),
        source_path: r.get(6)?,
        source_timestamp: r.get(7)?,
        object_hash: r.get(8)?,
        body_json: r.get(9)?,
        ai_cache_valid: r.get::<_, i64>(10)? != 0,
        ai_card_json: r.get(11)?,
    })
}

fn parse_source_kind(s: &str) -> SourceKind {
    match s {
        "file" => SourceKind::File,
        _ => SourceKind::Database,
    }
}

fn parse_primary_key_json(raw: Option<String>) -> Vec<String> {
    raw.and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;

    #[test]
    fn validation_index_reads_columns_without_body_json() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut users = table("users", &["id", "email"], db_source(Some(1), Some(2)));
        users.primary_key = vec!["id".into()];
        store.sync(&[users]).unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE catalog_object SET body_json='{}' WHERE object_key='dbo.users'",
                [],
            )
            .unwrap();

        let rows = store.live_validation_index_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].columns, vec!["id", "email"]);
    }
}
