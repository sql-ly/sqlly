//! ADR-011 pooled driver for the model engine: SQL Server connections that
//! run catalog SQL and return string rows. Pairs with
//! `sqlly_intel::modelengine::sql_collector::SqlCatalogCollector`.

use crate::tds::{TdsClient, connect, query_rows_on_client};
use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, SqlParam};
use sqlly_intel::modelengine::pool::{PoolError, PooledConnection, PooledDriver};

/// Connects catalog-collection connections for one profile.
pub struct TdsPooledDriver {
    pub profile: ConnectionProfile,
}

struct TdsPooledConn {
    client: TdsClient,
}

#[async_trait]
impl PooledDriver for TdsPooledDriver {
    async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError> {
        let client = connect(&self.profile).await.map_err(|e| e.to_string())?;
        Ok(Box::new(TdsPooledConn { client }))
    }
}

#[async_trait]
impl PooledConnection for TdsPooledConn {
    async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
        // No parameters → direct SQL batch (`simple_query`): catalog SQL and
        // fixture DDL alike run unprepared, matching how sqlly-tds executes
        // interactive batches (DDL such as CREATE VIEW cannot go through the
        // prepared-statement path).
        if params.is_empty() {
            let stream = self
                .client
                .simple_query(sql)
                .await
                .map_err(|e| e.to_string())?;
            let results = stream.into_results().await.map_err(|e| e.to_string())?;
            let Some(first) = results.into_iter().next() else {
                return Ok(Vec::new());
            };
            return Ok(first
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|data| cell_to_string(crate::tds::column_data_to_cell(data)))
                        .collect()
                })
                .collect());
        }
        let bound: Vec<SqlParam> = params.iter().map(|p| SqlParam::String(p.clone())).collect();
        let sets = query_rows_on_client(&mut self.client, sql, &bound)
            .await
            .map_err(|e| e.to_string())?;
        let Some(first) = sets.into_iter().next() else {
            return Ok(Vec::new());
        };
        Ok(first
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(cell_to_string).collect())
            .collect())
    }
}

fn cell_to_string(cell: CellValue) -> Option<String> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(if b { "1" } else { "0" }.to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(n) => Some(n.to_string()),
        CellValue::String(s) => Some(s),
        CellValue::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
    }
}
