//! FTS5-backed schema search and the search-document index it rides on.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};

use crate::catalog::documents::{DOCUMENT_GENERATOR_VERSION, generate_document};
use crate::catalog::fts::build_match_query;
use crate::catalog::wire::CatalogObject;

use super::objects::stored_object_from_row;
use super::{CatalogError, CatalogStore};

/// Optional filters for catalog search.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub object_types: Vec<String>,
}

/// A bounded schema-search result.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SchemaSearchHit {
    pub object_key: String,
    pub schema: Option<String>,
    pub name: String,
    pub object_type: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiny_card: Option<String>,
}

impl CatalogStore {
    /// Search live catalog objects using FTS5, returning a bounded result set.
    pub fn search_schema(
        &self,
        query: &str,
        limit: usize,
        filter: SearchFilter,
    ) -> Result<Vec<SchemaSearchHit>, CatalogError> {
        let Some(match_query) = build_match_query(query) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 100);
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT f.object_key, o.namespace, o.name, f.object_type,
                    -bm25(catalog_search_fts, 8.0, 1.0, 4.0) AS score,
                    c.content AS tiny_card
             FROM catalog_search_fts f
             JOIN catalog_object o ON o.object_key=f.object_key
             LEFT JOIN catalog_object_card c
                ON c.object_key=f.object_key AND c.card_type='tiny'
             WHERE catalog_search_fts MATCH ?1 AND o.is_deleted=0",
        );
        let normalized_types: Vec<String> = filter
            .object_types
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_ascii_lowercase())
            .collect();
        if !normalized_types.is_empty() {
            let placeholders = (0..normalized_types.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" AND f.object_type IN ({placeholders})"));
        }
        let limit_param = normalized_types.len() + 2;
        sql.push_str(&format!(
            " ORDER BY score DESC, o.name ASC LIMIT ?{limit_param}"
        ));

        let mut stmt = conn.prepare(&sql)?;
        let mut out = Vec::new();
        if normalized_types.is_empty() {
            let rows = stmt.query_map(
                params![match_query, limit as i64],
                schema_search_hit_from_row,
            )?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut values: Vec<&dyn ToSql> = Vec::with_capacity(normalized_types.len() + 2);
            values.push(&match_query);
            for object_type in &normalized_types {
                values.push(object_type);
            }
            let limit_i64 = limit as i64;
            values.push(&limit_i64);
            let rows = stmt.query_map(params_from_iter(values), schema_search_hit_from_row)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// True when every live object has a current generated document and FTS row.
    pub fn search_index_is_healthy(&self) -> Result<bool, CatalogError> {
        let conn = self.conn.lock();
        let (version, live_count, doc_count, fts_count) = search_index_counts(&conn)?;
        Ok(version >= DOCUMENT_GENERATOR_VERSION
            && live_count == doc_count
            && live_count == fts_count)
    }
}

fn schema_search_hit_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SchemaSearchHit> {
    Ok(SchemaSearchHit {
        object_key: r.get(0)?,
        schema: r.get(1)?,
        name: r.get(2)?,
        object_type: r.get(3)?,
        score: r.get(4)?,
        tiny_card: r.get(5)?,
    })
}

pub(super) fn ensure_search_index(conn: &Connection) -> Result<(), CatalogError> {
    let (version, live_count, doc_count, fts_count) = search_index_counts(conn)?;

    if version < DOCUMENT_GENERATOR_VERSION || live_count != doc_count || live_count != fts_count {
        reindex_search_documents(conn)?;
    }
    Ok(())
}

fn search_index_counts(conn: &Connection) -> Result<(i64, i64, i64, i64), CatalogError> {
    let version = conn
        .query_row(
            "SELECT value FROM catalog_meta WHERE key='search_doc_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let live_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM catalog_object WHERE is_deleted=0",
        [],
        |r| r.get(0),
    )?;
    let doc_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM catalog_search_document", [], |r| {
            r.get(0)
        })?;
    let fts_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM catalog_search_fts", [], |r| r.get(0))?;
    Ok((version, live_count, doc_count, fts_count))
}

fn reindex_search_documents(conn: &Connection) -> Result<(), CatalogError> {
    let now = Utc::now().timestamp_millis();
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM catalog_search_fts", [])?;
    tx.execute("DELETE FROM catalog_search_document", [])?;

    let mut objects = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT object_key, catalog, namespace, name, kind, source_kind,
                    source_path, source_timestamp, object_hash, body_json,
                    ai_cache_valid, ai_card_json
             FROM catalog_object WHERE is_deleted=0",
        )?;
        let rows = stmt.query_map([], stored_object_from_row)?;
        for row in rows {
            objects.push(row?);
        }
    }

    for stored in objects {
        let obj = stored.to_catalog_object()?;
        upsert_search_document(&tx, &obj, &stored.object_key, now)?;
    }
    tx.execute(
        "INSERT INTO catalog_meta(key,value) VALUES('search_doc_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![DOCUMENT_GENERATOR_VERSION.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn upsert_search_document(
    tx: &Connection,
    obj: &CatalogObject,
    key: &str,
    now: i64,
) -> Result<(), CatalogError> {
    let doc = generate_document(key, obj);
    delete_search_document(tx, key)?;
    // `content_hash` is persisted so future generator versions can skip row
    // rewrites when generated searchable text is unchanged.
    tx.execute(
        "INSERT INTO catalog_search_document
            (object_key, document_type, title, body, keywords, content_hash, generator_version, generated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            &doc.object_key,
            &doc.document_type,
            &doc.title,
            &doc.body,
            &doc.keywords,
            &doc.content_hash,
            doc.generator_version,
            now,
        ],
    )?;
    tx.execute(
        "INSERT INTO catalog_search_fts
            (title, body, keywords, object_key, object_type, namespace)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            &doc.title,
            &doc.body,
            &doc.keywords,
            key,
            obj.kind.as_str(),
            obj.schema.as_deref(),
        ],
    )?;
    Ok(())
}

pub(super) fn delete_search_document(tx: &Connection, key: &str) -> Result<(), CatalogError> {
    tx.execute(
        "DELETE FROM catalog_search_fts WHERE object_key=?1",
        params![key],
    )?;
    tx.execute(
        "DELETE FROM catalog_search_document WHERE object_key=?1",
        params![key],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;
    use crate::catalog::wire::CatalogObjectKind;

    #[test]
    fn search_finds_column_and_respects_tombstones() {
        let store = CatalogStore::open_in_memory().unwrap();
        let users = table(
            "users",
            &["id", "email_address"],
            db_source(Some(1), Some(2)),
        );
        let logs = table("logs", &["id", "message"], db_source(Some(1), Some(2)));
        store.sync(&[users.clone(), logs]).unwrap();
        assert_eq!(
            store
                .search_schema("email address", 10, SearchFilter::default())
                .unwrap()[0]
                .name,
            "users"
        );

        store
            .sync(&[table(
                "logs",
                &["id", "message"],
                db_source(Some(1), Some(2)),
            )])
            .unwrap();
        assert!(
            store
                .search_schema("email address", 10, SearchFilter::default())
                .unwrap()
                .is_empty()
        );

        store.sync(&[users]).unwrap();
        assert_eq!(
            store
                .search_schema("email address", 10, SearchFilter::default())
                .unwrap()[0]
                .name,
            "users"
        );
    }

    #[test]
    fn search_schema_returns_tiny_card_preview() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table(
                "users",
                &["id", "email_address"],
                db_source(Some(1), Some(2)),
            )])
            .unwrap();

        let hits = store
            .search_schema("email address", 10, SearchFilter::default())
            .unwrap();

        assert_eq!(hits[0].name, "users");
        assert_eq!(
            hits[0].tiny_card.as_deref(),
            Some("TABLE dbo.users: id, email_address")
        );
    }

    #[test]
    fn search_handles_special_characters_without_error() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["email"], db_source(Some(1), Some(2)))])
            .unwrap();
        assert!(
            store
                .search_schema("* - AND OR \"", 10, SearchFilter::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search_schema("email* - OR \"x\"", 10, SearchFilter::default())
                .unwrap()[0]
                .name,
            "users"
        );
    }

    #[test]
    fn search_filters_object_types() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut view = table("active_users", &["email"], db_source(Some(1), Some(2)));
        view.kind = CatalogObjectKind::View;
        store
            .sync(&[
                table("users", &["email"], db_source(Some(1), Some(2))),
                view,
            ])
            .unwrap();

        let hits = store
            .search_schema(
                "email",
                10,
                SearchFilter {
                    object_types: vec!["view".into()],
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "active_users");
    }

    #[test]
    fn open_backfills_missing_search_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        {
            let store = CatalogStore::open_path(&path).unwrap();
            store
                .sync(&[table(
                    "users",
                    &["email_address"],
                    db_source(Some(1), Some(2)),
                )])
                .unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("DELETE FROM catalog_search_fts", []).unwrap();
            conn.execute("DELETE FROM catalog_search_document", [])
                .unwrap();
        }

        let store = CatalogStore::open_path(&path).unwrap();
        let hits = store
            .search_schema("email address", 10, SearchFilter::default())
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "users");
        assert!(store.search_index_is_healthy().unwrap());
    }

    #[test]
    fn search_finds_unique_definition_body_token() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut view = table("customer_summary", &["id"], db_source(Some(1), Some(2)));
        view.kind = CatalogObjectKind::View;
        view.definition = Some("CREATE VIEW dbo.customer_summary AS SELECT rare_invoice_rollup_token FROM dbo.invoices".into());
        store
            .sync(&[
                table("customers", &["id"], db_source(Some(1), Some(2))),
                view,
            ])
            .unwrap();

        let hits = store
            .search_schema("rare invoice rollup token", 10, SearchFilter::default())
            .unwrap();

        assert_eq!(hits[0].name, "customer_summary");
    }

    #[test]
    fn search_large_catalog_smoke_stays_bounded_and_relevant() {
        let store = CatalogStore::open_in_memory().unwrap();
        let mut objects = (0..250)
            .map(|i| {
                table(
                    &format!("table_{i:03}"),
                    &["id"],
                    db_source(Some(1), Some(2)),
                )
            })
            .collect::<Vec<_>>();
        objects.push(table(
            "target_orders",
            &["id", "target_customer_email"],
            db_source(Some(1), Some(2)),
        ));
        store.sync(&objects).unwrap();

        let hits = store
            .search_schema("target customer email", 10, SearchFilter::default())
            .unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "target_orders");
        assert!(hits.len() <= 10);
    }
}
