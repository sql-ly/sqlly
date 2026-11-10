//! ADR-011 pooled driver for the model engine: PostgreSQL connections that
//! run catalog SQL and return string rows. Pairs with
//! `sqlly_intel::modelengine::sql_collector::SqlCatalogCollector`.

use crate::pg::{PgClient, open_client};
use async_trait::async_trait;
use sqlly_core::ConnectionProfile;
use sqlly_intel::modelengine::pool::{PoolError, PooledConnection, PooledDriver};

/// Connects catalog-collection connections for one profile.
pub struct PgPooledDriver {
    pub profile: ConnectionProfile,
}

struct PgPooledConn {
    client: PgClient,
}

#[async_trait]
impl PooledDriver for PgPooledDriver {
    async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError> {
        let client = open_client(&self.profile)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Box::new(PgPooledConn { client }))
    }
}

#[async_trait]
impl PooledConnection for PgPooledConn {
    async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
        if !params.is_empty() {
            // Catalog SQL embeds escaped literals; the simple-query protocol
            // has no parameter binding, so be honest rather than interpolate.
            return Err("postgres catalog connections do not bind parameters".into());
        }
        let messages = self
            .client
            .client
            .simple_query(sql)
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for message in messages {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = message {
                out.push(
                    (0..row.len())
                        .map(|i| row.get(i).map(str::to_string))
                        .collect(),
                );
            }
        }
        Ok(out)
    }
}
