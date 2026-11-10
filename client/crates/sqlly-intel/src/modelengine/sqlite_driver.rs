//! SQLite pooled driver + catalog collector. The first concrete
//! [`PooledDriver`] (ADR-011 delivery phase 1 dialect) — used in production
//! for SQLite sources and in tests as the live-database fixture that needs no
//! container. rusqlite is synchronous, so queries hop to `spawn_blocking`.

use crate::modelengine::collect::{CatalogCollector, CollectUnit};
use crate::modelengine::pool::{ConnectionPool, PoolError, PooledConnection, PooledDriver};
use async_trait::async_trait;
use sqlly_lang::sqlmodel::{Dialect, ObjectFragment, Provenance};
use sqlly_lang::sqlparse::{lower, parse};
use std::sync::Arc;

pub struct SqliteDriver {
    pub path: String,
}

struct SqliteConn {
    // Connection lives on its own blocking task via a channel so the async
    // trait object stays Send.
    tx: tokio::sync::mpsc::Sender<Request>,
}

type QueryResult = Result<Vec<Vec<Option<String>>>, PoolError>;

struct Request {
    sql: String,
    params: Vec<String>,
    respond: tokio::sync::oneshot::Sender<QueryResult>,
}

#[async_trait]
impl PooledDriver for SqliteDriver {
    async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError> {
        let path = self.path.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Request>(8);
        // One blocking thread owns the rusqlite connection for its lifetime.
        let ready = tokio::task::spawn_blocking(move || -> Result<(), PoolError> {
            let conn = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
            while let Some(req) = rx.blocking_recv() {
                let result = run_query(&conn, &req.sql, &req.params);
                let _ = req.respond.send(result);
            }
            Ok(())
        });
        // The spawn itself can't fail synchronously; verify the open by
        // running a probe query.
        let conn = SqliteConn { tx };
        let mut boxed: Box<dyn PooledConnection> = Box::new(conn);
        match boxed.query("select 1", &[]).await {
            Ok(_) => Ok(boxed),
            Err(e) => {
                drop(boxed);
                let _ = ready.await;
                Err(e)
            }
        }
    }
}

fn run_query(conn: &rusqlite::Connection, sql: &str, params: &[String]) -> QueryResult {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let col_count = stmt.column_count();
    let params: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut rows = stmt.query(&params[..]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut record = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let value: Option<String> = match row.get_ref(i).map_err(|e| e.to_string())? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(v) => Some(v.to_string()),
                rusqlite::types::ValueRef::Real(v) => Some(v.to_string()),
                rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).to_string()),
                rusqlite::types::ValueRef::Blob(b) => Some(format!("<blob {} bytes>", b.len())),
            };
            record.push(value);
        }
        out.push(record);
    }
    Ok(out)
}

#[async_trait]
impl PooledConnection for SqliteConn {
    async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
        let (respond, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Request {
                sql: sql.to_string(),
                params: params.to_vec(),
                respond,
            })
            .await
            .map_err(|_| "sqlite worker gone".to_string())?;
        rx.await
            .map_err(|_| "sqlite worker dropped request".to_string())?
    }
}

// ---------------------------------------------------------------------------
// Catalog collector
// ---------------------------------------------------------------------------

/// SQLite catalog collection: `sqlite_master` stores the original CREATE
/// statements, so collection parses them with our own parser — exercising the
/// same path file collection uses.
pub struct SqliteCatalogCollector;

#[async_trait]
impl CatalogCollector for SqliteCatalogCollector {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn plan(&self, _pool: &Arc<ConnectionPool>) -> Result<Vec<CollectUnit>, PoolError> {
        // SQLite has one implicit namespace; one unit per object kind batch.
        Ok(vec![
            CollectUnit {
                label: "sqlite:tables".into(),
                region_prefix: String::new(),
            },
            CollectUnit {
                label: "sqlite:views".into(),
                region_prefix: String::new(),
            },
        ])
    }

    async fn collect(
        &self,
        pool: &Arc<ConnectionPool>,
        unit: &CollectUnit,
        profile: &str,
    ) -> Result<Vec<ObjectFragment>, PoolError> {
        let kind = if unit.label.ends_with("tables") {
            "table"
        } else {
            "view"
        };
        let mut conn = pool.acquire().await?;
        let rows = conn
            .query(
                "select sql from sqlite_master where type = ?1 and name not like 'sqlite_%' and sql is not null",
                &[kind.to_string()],
            )
            .await?;
        let mut fragments = Vec::new();
        for row in rows {
            if let Some(Some(sql)) = row.first().cloned() {
                let tree = parse(&sql, Dialect::Sqlite);
                fragments.extend(lower(
                    &tree,
                    Provenance::Database {
                        profile: profile.to_string(),
                    },
                ));
            }
        }
        Ok(fragments)
    }
}
