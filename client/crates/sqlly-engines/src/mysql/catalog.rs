//! ADR-011 pooled driver for the model engine: MySQL/MariaDB connections that
//! run catalog SQL and return string rows. Pairs with
//! `sqlly_intel::modelengine::sql_collector::SqlCatalogCollector`.

use crate::mysql::{MySqlClient, open_client};
use async_trait::async_trait;
use mysql_async::Value;
use mysql_async::prelude::Queryable;
use sqlly_core::ConnectionProfile;
use sqlly_intel::modelengine::pool::{PoolError, PooledConnection, PooledDriver};

/// Connects catalog-collection connections for one profile.
pub struct MySqlPooledDriver {
    pub profile: ConnectionProfile,
}

struct MySqlPooledConn {
    client: MySqlClient,
}

#[async_trait]
impl PooledDriver for MySqlPooledDriver {
    async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError> {
        let client = open_client(&self.profile)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Box::new(MySqlPooledConn { client }))
    }
}

#[async_trait]
impl PooledConnection for MySqlPooledConn {
    async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
        let mut conn = self.client.conn.lock().await;
        let rows: Vec<mysql_async::Row> = if params.is_empty() {
            conn.query(sql).await.map_err(|e| e.to_string())?
        } else {
            let bound = mysql_async::Params::Positional(
                params
                    .iter()
                    .map(|p| Value::Bytes(p.clone().into_bytes()))
                    .collect(),
            );
            conn.exec(sql, bound).await.map_err(|e| e.to_string())?
        };
        Ok(rows
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| row.as_ref(i).and_then(value_to_string))
                    .collect()
            })
            .collect())
    }
}

/// Catalog cells as strings: identifiers and counts, never binary payloads.
fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(f) => Some(f.to_string()),
        Value::Date(y, mo, d, h, mi, s, _) => {
            Some(format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"))
        }
        Value::Time(neg, days, h, mi, s, _) => {
            let sign = if *neg { "-" } else { "" };
            Some(format!(
                "{sign}{}:{mi:02}:{s:02}",
                u32::from(*h) + days * 24
            ))
        }
    }
}
