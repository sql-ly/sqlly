//! Oracle Database row-editing lane: the live half of the Oracle support —
//! provenance resolution against the catalog, table loading from `ALL_*`
//! views, and the transactional statement execution — plugged into the
//! engine-generic flows of [`crate::row_editing_engines`]. Every statement
//! it runs is built by the pure [`crate::row_editing::oracle_sql`] module.
//!
//! Provenance: the statement is parsed for its single source table
//! (`oracle_sql::single_source_table`), resolved through `ALL_OBJECTS` (one
//! level of synonym allowed), then the server describes the projection with
//! a `WHERE 1 = 0` wrapper that runs nothing; projected names that match
//! table columns are editable, everything else is read-only. Inserts return
//! the new `ROWID` through an OUT bind and refetch the row inside the open
//! transaction so sequence / identity / default values come back resolved.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;

use async_trait::async_trait;
use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioResult};
use sqlly_engines::oracle::{
    OracleClient, execute_params_on_client, execute_returning_text_on_client, query_rows_on_client,
};

use crate::row_editing::oracle_sql::{
    OracleTableColumn, ROWID_COLUMN, ROWID_OUT_BIND, base_type, build_delete_sql, build_insert_sql,
    build_select_row_sql, build_update_sql, describe_wrapper_sql, row_edit_columns,
    single_source_table,
};
use crate::row_editing::{
    CheckConstraintSpec, DeleteRowRequest, DescribeColumn, FkColumnRow, ForeignKeySpec,
    IncomingFkColumnRow, IncomingForeignKey, InsertRowRequest, LoadedTable, TableTarget,
    UpdateRowRequest, group_foreign_keys, group_incoming_foreign_keys,
};
use crate::row_editing_engines::RowEditEngine;
use crate::row_editing_sql as dialect;

pub use crate::row_editing::oracle_sql::session_setup_sql;

pub(crate) struct OracleEngine<'a> {
    client: &'a OracleClient,
    db: DatabaseType,
    /// Per-table column cache for this operation's lifetime.
    types: HashMap<(String, String), Vec<OracleTableColumn>>,
    current_schema: Option<String>,
}

impl<'a> OracleEngine<'a> {
    pub(crate) fn new(client: &'a OracleClient, db: DatabaseType) -> Self {
        Self {
            client,
            db,
            types: HashMap::new(),
            current_schema: None,
        }
    }

    async fn run_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let sets = query_rows_on_client(self.client, sql, params).await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    async fn current_schema(&mut self) -> StudioResult<String> {
        if let Some(schema) = &self.current_schema {
            return Ok(schema.clone());
        }
        let set = self
            .run_query(
                "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM DUAL",
                &[],
            )
            .await?;
        let schema = set
            .rows
            .first()
            .and_then(|row| cell_str(row.first()))
            .unwrap_or_default();
        self.current_schema = Some(schema.clone());
        Ok(schema)
    }

    /// The table's columns (`ALL_TAB_COLS`, visible columns only), cached
    /// per operation. Empty when the table does not exist or is invisible.
    async fn table_types(&mut self, target: &TableTarget) -> StudioResult<Vec<OracleTableColumn>> {
        let key = (target.schema.clone(), target.table.clone());
        if let Some(cached) = self.types.get(&key) {
            return Ok(cached.clone());
        }
        let set = self
            .run_query(
                "SELECT c.column_name, c.data_type, c.char_length, c.data_precision, c.data_scale, \
                 CASE c.nullable WHEN 'Y' THEN 1 ELSE 0 END, \
                 CASE c.identity_column WHEN 'YES' THEN 1 ELSE 0 END, \
                 CASE c.virtual_column WHEN 'YES' THEN 1 ELSE 0 END, \
                 c.data_default \
                 FROM all_tab_cols c WHERE c.owner = :1 AND c.table_name = :2 \
                 AND c.hidden_column = 'NO' ORDER BY c.column_id",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        let columns: Vec<OracleTableColumn> = set
            .rows
            .iter()
            .filter_map(|row| {
                Some(OracleTableColumn {
                    name: cell_str(row.first())?,
                    data_type: cell_str(row.get(1)).unwrap_or_default(),
                    char_len: cell_i64(row.get(2)),
                    precision: cell_i64(row.get(3)),
                    scale: cell_i64(row.get(4)),
                    nullable: cell_bool(row.get(5)),
                    is_identity: cell_bool(row.get(6)),
                    is_virtual: cell_bool(row.get(7)),
                    default_definition: cell_str(row.get(8)).map(|d| d.trim().to_string()),
                })
            })
            .collect();
        self.types.insert(key, columns.clone());
        Ok(columns)
    }

    /// Resolve a parsed `(schema, table)` to a base table, following one
    /// level of synonym. `Err(reason)` is the user-facing "not editable".
    async fn resolve_table(
        &mut self,
        schema: Option<String>,
        table: String,
    ) -> StudioResult<Result<TableTarget, sqlly_core::DisplayMessage>> {
        let schema = match schema {
            Some(schema) => schema,
            None => self.current_schema().await?,
        };
        let quoted = dialect::quote_table(
            self.db,
            &TableTarget {
                schema: schema.clone(),
                table: table.clone(),
            },
        );
        let set = self
            .run_query(
                "SELECT object_type FROM all_objects WHERE owner = :1 AND object_name = :2 \
                 AND object_type IN ('TABLE', 'VIEW', 'SYNONYM', 'MATERIALIZED VIEW')",
                &[
                    SqlParam::String(schema.clone()),
                    SqlParam::String(table.clone()),
                ],
            )
            .await?;
        let mut synonym_owner = schema.clone();
        let mut object_type = set.rows.first().and_then(|row| cell_str(row.first()));
        if object_type.is_none() {
            let set = self
                .run_query(
                    "SELECT 1 FROM all_synonyms WHERE owner = 'PUBLIC' AND synonym_name = :1",
                    &[SqlParam::String(table.clone())],
                )
                .await?;
            if !set.rows.is_empty() {
                object_type = Some("SYNONYM".to_string());
                synonym_owner = "PUBLIC".to_string();
            }
        }
        match object_type.as_deref() {
            Some("TABLE") => Ok(Ok(TableTarget { schema, table })),
            Some("SYNONYM") => {
                let set = self
                    .run_query(
                        "SELECT table_owner, table_name FROM all_synonyms WHERE owner = :1 AND synonym_name = :2",
                        &[SqlParam::String(synonym_owner), SqlParam::String(table.clone())],
                    )
                    .await?;
                let Some(row) = set.rows.first() else {
                    return Ok(Err(crate::row_editing::not_editable::synonym_unresolved(
                        &quoted,
                    )));
                };
                let target = TableTarget {
                    schema: cell_str(row.first()).unwrap_or_default(),
                    table: cell_str(row.get(1)).unwrap_or_default(),
                };
                let set = self
                    .run_query(
                        "SELECT 1 FROM all_tables WHERE owner = :1 AND table_name = :2",
                        &[
                            SqlParam::String(target.schema.clone()),
                            SqlParam::String(target.table.clone()),
                        ],
                    )
                    .await?;
                if set.rows.is_empty() {
                    return Ok(Err(
                        crate::row_editing::not_editable::synonym_not_base_table(&quoted),
                    ));
                }
                Ok(Ok(target))
            }
            Some(_) => Ok(Err(crate::row_editing::not_editable::not_base_table(
                &quoted,
            ))),
            None => Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            ))),
        }
    }

    /// Best-effort display column for a referenced table: a VARCHAR2/CHAR
    /// column with a name-like identifier.
    async fn display_column_for(&mut self, target: &TableTarget) -> Option<String> {
        let columns = self.table_types(target).await.ok()?;
        let candidates = [
            "name",
            "displayname",
            "display_name",
            "fullname",
            "full_name",
            "title",
            "label",
            "description",
        ];
        let mut best: Option<(usize, String)> = None;
        for column in &columns {
            let base = base_type(&column.data_type);
            if !matches!(base.as_str(), "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR") {
                continue;
            }
            let lower = column.name.to_lowercase();
            if let Some(rank) = candidates.iter().position(|c| *c == lower)
                && best.as_ref().is_none_or(|(r, _)| rank < *r)
            {
                best = Some((rank, column.name.clone()));
            }
        }
        best.map(|(_, name)| name)
    }

    async fn load_fk_rows(&mut self, target: &TableTarget) -> StudioResult<Vec<FkColumnRow>> {
        let set = self
            .run_query(
                "SELECT c.constraint_name, r.owner, r.table_name, cc.column_name, rc.column_name \
                 FROM all_constraints c \
                 JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'R' \
                 ORDER BY c.constraint_name, cc.position",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        let mut rows: Vec<FkColumnRow> = Vec::new();
        let mut display_cache: HashMap<(String, String), Option<String>> = HashMap::new();
        for row in &set.rows {
            let Some(constraint_name) = cell_str(row.first()) else {
                continue;
            };
            let ref_schema = cell_str(row.get(1)).unwrap_or_default();
            let ref_table = cell_str(row.get(2)).unwrap_or_default();
            let key = (ref_schema.clone(), ref_table.clone());
            let display_column = match display_cache.get(&key) {
                Some(cached) => cached.clone(),
                None => {
                    let display = self
                        .display_column_for(&TableTarget {
                            schema: ref_schema.clone(),
                            table: ref_table.clone(),
                        })
                        .await;
                    display_cache.insert(key, display.clone());
                    display
                }
            };
            rows.push(FkColumnRow {
                constraint_name,
                ref_schema,
                ref_table,
                parent_column: cell_str(row.get(3)).unwrap_or_default(),
                referenced_column: cell_str(row.get(4)).unwrap_or_default(),
                display_column,
            });
        }
        Ok(rows)
    }
}

fn cell_str(cell: Option<&CellValue>) -> Option<String> {
    match cell {
        Some(CellValue::String(s)) => Some(s.clone()),
        Some(CellValue::I64(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn cell_i64(cell: Option<&CellValue>) -> Option<i64> {
    match cell {
        Some(CellValue::I64(n)) => Some(*n),
        Some(CellValue::F64(f)) => Some(*f as i64),
        Some(CellValue::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn cell_bool(cell: Option<&CellValue>) -> bool {
    matches!(cell, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
}

#[async_trait]
impl RowEditEngine for OracleEngine<'_> {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, sqlly_core::DisplayMessage>> {
        let (schema, table) = match single_source_table(sql) {
            Ok(parsed) => parsed,
            Err(reason) => return Ok(Err(reason)),
        };
        let target = match self.resolve_table(schema, table).await? {
            Ok(target) => target,
            Err(reason) => return Ok(Err(reason)),
        };
        let projection =
            match query_rows_on_client(self.client, &describe_wrapper_sql(sql), &[]).await {
                Ok(sets) => sets.into_iter().next().unwrap_or_default(),
                Err(err) => {
                    return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                        err,
                    )));
                }
            };
        let types = self.table_types(&target).await?;
        Ok(Ok(projection
            .columns
            .iter()
            .map(|column| {
                let is_rowid = column.name.eq_ignore_ascii_case(ROWID_COLUMN);
                let source = types
                    .iter()
                    .find(|c| c.name == column.name)
                    .map(|c| c.name.clone())
                    .or_else(|| is_rowid.then(|| ROWID_COLUMN.to_string()));
                DescribeColumn {
                    name: column.name.clone(),
                    is_hidden: false,
                    source_schema: source.as_ref().map(|_| target.schema.clone()),
                    source_table: source.as_ref().map(|_| target.table.clone()),
                    source_column: source,
                }
            })
            .collect()))
    }

    async fn load_table(
        &mut self,
        target: &TableTarget,
    ) -> StudioResult<Result<LoadedTable, sqlly_core::DisplayMessage>> {
        let quoted = dialect::quote_table(self.db, target);
        let name_params = [
            SqlParam::String(target.schema.clone()),
            SqlParam::String(target.table.clone()),
        ];
        let set = self
            .run_query(
                "SELECT 1 FROM all_tables WHERE owner = :1 AND table_name = :2",
                &name_params,
            )
            .await?;
        if set.rows.is_empty() {
            let set = self
                .run_query(
                    "SELECT 1 FROM all_objects WHERE owner = :1 AND object_name = :2",
                    &name_params,
                )
                .await?;
            return Ok(Err(if set.rows.is_empty() {
                crate::row_editing::not_editable::table_not_found(&quoted)
            } else {
                crate::row_editing::not_editable::not_base_table(&quoted)
            }));
        }
        let types = self.table_types(target).await?;
        if types.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        }

        let set = self
            .run_query(
                "SELECT cc.column_name FROM all_constraints c \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'P' AND c.status = 'ENABLED' \
                 ORDER BY cc.position",
                &name_params,
            )
            .await?;
        let pk: Vec<String> = set
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect();
        let (columns, key_columns) = row_edit_columns(&types, &pk);

        let fk_rows = self.load_fk_rows(target).await?;
        let foreign_keys: Vec<ForeignKeySpec> = group_foreign_keys(&fk_rows);

        let set = self
            .run_query(
                "SELECT c.constraint_name, c.search_condition FROM all_constraints c \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'C' \
                 AND c.status = 'ENABLED' AND c.generated = 'USER NAME' ORDER BY c.constraint_name",
                &name_params,
            )
            .await?;
        let check_constraints: Vec<CheckConstraintSpec> = set
            .rows
            .iter()
            .filter_map(|row| {
                Some(CheckConstraintSpec {
                    name: cell_str(row.first())?,
                    definition: cell_str(row.get(1))?,
                })
            })
            .collect();

        let set = self
            .run_query(
                "SELECT c.constraint_name, c.owner, c.table_name, cc.column_name, rc.column_name \
                 FROM all_constraints c \
                 JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
                 WHERE r.owner = :1 AND r.table_name = :2 AND c.constraint_type = 'R' \
                 ORDER BY c.constraint_name, cc.position",
                &name_params,
            )
            .await?;
        let incoming_rows: Vec<IncomingFkColumnRow> = set
            .rows
            .iter()
            .filter_map(|row| {
                Some(IncomingFkColumnRow {
                    constraint_name: cell_str(row.first())?,
                    child_schema: cell_str(row.get(1)).unwrap_or_default(),
                    child_table: cell_str(row.get(2)).unwrap_or_default(),
                    child_column: cell_str(row.get(3)).unwrap_or_default(),
                    this_column: cell_str(row.get(4)).unwrap_or_default(),
                })
            })
            .collect();
        let incoming_foreign_keys: Vec<IncomingForeignKey> =
            group_incoming_foreign_keys(&incoming_rows);

        Ok(Ok(LoadedTable {
            columns,
            key_columns,
            foreign_keys,
            check_constraints,
            incoming_foreign_keys,
        }))
    }

    async fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        self.run_query(sql, params).await
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64> {
        execute_params_on_client(self.client, sql, params).await
    }

    /// Oracle transactions begin implicitly with the first statement; the
    /// lane's session runs with autocommit off, so there is nothing to send.
    async fn begin(&mut self) -> StudioResult<()> {
        Ok(())
    }

    async fn commit(&mut self) -> StudioResult<()> {
        execute_params_on_client(self.client, "COMMIT", &[])
            .await
            .map(|_| ())
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        execute_params_on_client(self.client, "ROLLBACK", &[])
            .await
            .map(|_| ())
    }

    async fn build_select_row(
        &mut self,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(target).await?;
        Ok(build_select_row_sql(self.db, target, key, &types))
    }

    async fn build_update(
        &mut self,
        request: &UpdateRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(&request.target).await?;
        Ok(build_update_sql(self.db, request, &types))
    }

    async fn build_delete(
        &mut self,
        request: &DeleteRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(&request.target).await?;
        Ok(build_delete_sql(self.db, request, &types))
    }

    /// INSERT with `RETURNING ROWID INTO :sqlly_rowid`, then a refetch of
    /// that row inside the open transaction so identity, sequence and
    /// default values come back resolved.
    async fn insert_returning(
        &mut self,
        request: &InsertRowRequest,
    ) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
        let types = self.table_types(&request.target).await?;
        let (sql, params) = build_insert_sql(self.db, request, &types, true);
        let (affected, rowid) =
            execute_returning_text_on_client(self.client, &sql, &params, ROWID_OUT_BIND).await?;
        if affected != 1 {
            return Err(crate::row_editing::errors::insert_affected_rows(affected));
        }
        let Some(rowid) = rowid else {
            return Err(crate::row_editing::errors::oracle_insert_without_rowid());
        };
        let key = vec![(ROWID_COLUMN.to_string(), CellValue::String(rowid))];
        let (fetch, params) = build_select_row_sql(self.db, &request.target, &key, &types);
        let set = self.run_query(&fetch, &params).await?;
        crate::row_editing_engines::expect_single_inserted_row(set)
    }
}
