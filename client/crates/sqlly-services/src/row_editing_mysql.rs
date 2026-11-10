//! MySQL/MariaDB row-editing engine.
//!
//! Provenance comes from the server's own description of the prepared
//! statement: the MySQL protocol's column definitions carry the source
//! schema, original table, and original column name for every projected
//! column (empty for expressions) — the MySQL equivalent of
//! `sp_describe_first_result_set`. Table metadata comes from
//! `information_schema` (`TABLES`, `COLUMNS`, `STATISTICS` for primary-key
//! membership and order, `KEY_COLUMN_USAGE` for foreign keys both ways,
//! `CHECK_CONSTRAINTS` best-effort on servers that have it).
//!
//! MySQL has no `RETURNING`, so `insert_returning` executes the INSERT and
//! refetches the row inside the same transaction: through
//! `LAST_INSERT_ID()` when an auto-increment key column was left for the
//! server to assign, through the supplied primary-key values when the full
//! key was provided, and by echoing the supplied values when the table gives
//! no way to address the new row.
//!
//! The service opens these clients with `CLIENT_FOUND_ROWS` so a guarded
//! UPDATE reports rows *matched* — without it, an update that leaves values
//! byte-identical would misreport as a concurrency conflict.

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioResult};
use std::collections::HashMap;

use crate::row_editing::{
    CheckConstraintSpec, DescribeColumn, FkColumnRow, IncomingFkColumnRow, InsertRowRequest,
    LoadedTable, RowEditColumn, TableTarget, fill_enum_values_from_checks, group_foreign_keys,
    group_incoming_foreign_keys, parse_mysql_enum_list,
};
use crate::row_editing_engines::RowEditEngine;
use crate::row_editing_sql as dialect;

/// The key facts about one column that the insert-refetch strategy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MySqlKeyColumn {
    pub name: String,
    pub is_primary: bool,
    pub is_auto_increment: bool,
}

/// How to locate a freshly inserted row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InsertRefetch {
    /// `WHERE auto_col = LAST_INSERT_ID()` — an auto-increment key column was
    /// left for the server to assign.
    ByAutoIncrement(String),
    /// The full primary key was supplied with non-NULL values.
    ByKey(Vec<(String, CellValue)>),
    /// No addressable key: echo the supplied values back.
    Echo,
}

/// Decides the refetch strategy from catalog key facts and the supplied
/// values. Pure and unit-tested.
pub(crate) fn insert_refetch_plan(
    columns: &[MySqlKeyColumn],
    values: &[(String, CellValue)],
) -> InsertRefetch {
    let supplied = |name: &str| {
        values
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    };
    if let Some(auto) = columns.iter().find(|c| c.is_primary && c.is_auto_increment)
        && !matches!(supplied(&auto.name), Some(value) if !matches!(value, CellValue::Null))
    {
        // The server assigned the value (omitted or explicit NULL).
        return InsertRefetch::ByAutoIncrement(auto.name.clone());
    }
    let key: Vec<&MySqlKeyColumn> = columns.iter().filter(|c| c.is_primary).collect();
    if !key.is_empty() {
        let mut pairs = Vec::with_capacity(key.len());
        for column in &key {
            match supplied(&column.name) {
                Some(value) if !matches!(value, CellValue::Null) => {
                    pairs.push((column.name.clone(), value.clone()));
                }
                _ => return InsertRefetch::Echo,
            }
        }
        return InsertRefetch::ByKey(pairs);
    }
    InsertRefetch::Echo
}

pub(crate) struct MySqlEngine<'a> {
    client: &'a sqlly_engines::mysql::MySqlClient,
    db: DatabaseType,
}

impl<'a> MySqlEngine<'a> {
    pub(crate) fn new(client: &'a sqlly_engines::mysql::MySqlClient, db: DatabaseType) -> Self {
        Self { client, db }
    }

    async fn run_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let sets = sqlly_engines::mysql::query_rows_on_client(self.client, sql, params).await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    async fn key_facts(&self, target: &TableTarget) -> StudioResult<Vec<MySqlKeyColumn>> {
        let set = self
            .run_query(
                "SELECT COLUMN_NAME, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| {
                Some(MySqlKeyColumn {
                    name: cell_str(row.first())?,
                    is_primary: cell_str(row.get(1))
                        .is_some_and(|key| key.eq_ignore_ascii_case("PRI")),
                    is_auto_increment: cell_str(row.get(2))
                        .is_some_and(|extra| extra.to_ascii_lowercase().contains("auto_increment")),
                })
            })
            .collect())
    }
}

fn cell_str(cell: Option<&CellValue>) -> Option<String> {
    match cell {
        Some(CellValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn cell_i64(cell: Option<&CellValue>) -> Option<i64> {
    match cell {
        Some(CellValue::I64(n)) => Some(*n),
        _ => None,
    }
}

#[async_trait]
impl RowEditEngine for MySqlEngine<'_> {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, sqlly_core::DisplayMessage>> {
        let origins =
            match sqlly_engines::mysql::describe_statement_on_client(self.client, sql).await {
                Ok(origins) => origins,
                Err(err) => {
                    return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                        err,
                    )));
                }
            };
        Ok(Ok(origins
            .iter()
            .map(|origin| {
                let sourced = !origin.schema.is_empty()
                    && !origin.base_table.is_empty()
                    && !origin.origin_column.is_empty();
                DescribeColumn {
                    name: origin.name.clone(),
                    is_hidden: false,
                    source_schema: sourced.then(|| origin.schema.clone()),
                    source_table: sourced.then(|| origin.base_table.clone()),
                    source_column: sourced.then(|| origin.origin_column.clone()),
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
                "SELECT TABLE_TYPE FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                &name_params,
            )
            .await?;
        let Some(row) = set.rows.first() else {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        };
        let table_type = cell_str(row.first()).unwrap_or_default();
        if !table_type.eq_ignore_ascii_case("BASE TABLE") {
            return Ok(Err(crate::row_editing::not_editable::not_base_table(
                &quoted,
            )));
        }

        let set = self
            .run_query(
                "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, CHARACTER_MAXIMUM_LENGTH, \
                 NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE, EXTRA, COLUMN_DEFAULT \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                &name_params,
            )
            .await?;
        let columns: Vec<RowEditColumn> = set
            .rows
            .iter()
            .filter_map(|row| {
                let name = cell_str(row.first())?;
                let data_type = cell_str(row.get(1)).unwrap_or_default();
                let column_type_raw = cell_str(row.get(2)).unwrap_or_default();
                let column_type = column_type_raw.to_lowercase();
                // `tinyint(1)` is MySQL's boolean spelling; preserve it so the
                // editor renders a true/false field instead of a 0-255 number.
                let data_type = if column_type.starts_with("tinyint(1)") {
                    "tinyint(1)".to_string()
                } else {
                    data_type
                };
                let extra = cell_str(row.get(7)).unwrap_or_default().to_lowercase();
                Some(RowEditColumn {
                    name,
                    data_type,
                    char_max_length: cell_i64(row.get(3)),
                    precision: cell_i64(row.get(4)).map(|p| p as u8),
                    scale: cell_i64(row.get(5)).map(|s| s as u8),
                    nullable: cell_str(row.get(6)).is_some_and(|n| n.eq_ignore_ascii_case("YES")),
                    is_primary_key: false, // filled in below
                    is_identity: extra.contains("auto_increment"),
                    is_computed: extra.contains("virtual generated")
                        || extra.contains("stored generated"),
                    is_rowversion: false,
                    default_definition: cell_str(row.get(8)),
                    result_ordinal: None,
                    enum_values: parse_mysql_enum_list(&column_type_raw),
                })
            })
            .collect();
        if columns.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        }

        // Primary-key membership in key order (information_schema.STATISTICS
        // is the ordered view over COLUMN_KEY = 'PRI').
        let key_columns: Vec<String> = self
            .run_query(
                "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = 'PRIMARY' \
                 ORDER BY SEQ_IN_INDEX",
                &name_params,
            )
            .await?
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect();
        let mut columns = columns;
        for column in &mut columns {
            column.is_primary_key = key_columns.contains(&column.name);
        }

        let fk_set = self
            .run_query(
                "SELECT CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, \
                 COLUMN_NAME, REFERENCED_COLUMN_NAME \
                 FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
                &name_params,
            )
            .await?;
        let mut fk_rows: Vec<FkColumnRow> = fk_set
            .rows
            .iter()
            .filter_map(|row| {
                Some(FkColumnRow {
                    constraint_name: cell_str(row.first())?,
                    ref_schema: cell_str(row.get(1)).unwrap_or_default(),
                    ref_table: cell_str(row.get(2)).unwrap_or_default(),
                    parent_column: cell_str(row.get(3)).unwrap_or_default(),
                    referenced_column: cell_str(row.get(4)).unwrap_or_default(),
                    display_column: None,
                })
            })
            .collect();
        let mut display_cache: HashMap<(String, String), Option<String>> = HashMap::new();
        for row in &mut fk_rows {
            let key = (row.ref_schema.clone(), row.ref_table.clone());
            if !display_cache.contains_key(&key) {
                let display = self
                    .run_query(
                        "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                         AND DATA_TYPE IN \
                         ('varchar','char','text','tinytext','mediumtext','longtext') \
                         AND LOWER(COLUMN_NAME) IN \
                         ('name','displayname','fullname','title','label','description') \
                         ORDER BY CASE LOWER(COLUMN_NAME) WHEN 'name' THEN 0 \
                         WHEN 'displayname' THEN 1 WHEN 'fullname' THEN 2 WHEN 'title' THEN 3 \
                         WHEN 'label' THEN 4 WHEN 'description' THEN 5 ELSE 6 END, \
                         ORDINAL_POSITION LIMIT 1",
                        &[
                            SqlParam::String(key.0.clone()),
                            SqlParam::String(key.1.clone()),
                        ],
                    )
                    .await
                    .ok()
                    .and_then(|set| set.rows.first().and_then(|r| cell_str(r.first())));
                display_cache.insert(key.clone(), display);
            }
            row.display_column = display_cache.get(&key).cloned().flatten();
        }
        let foreign_keys = group_foreign_keys(&fk_rows);

        // CHECK constraints exist on MySQL 8.0.16+/MariaDB 10.2+; best-effort
        // (older servers simply have none to report).
        let check_constraints: Vec<CheckConstraintSpec> = match self
            .run_query(
                "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE \
                 FROM information_schema.CHECK_CONSTRAINTS cc \
                 JOIN information_schema.TABLE_CONSTRAINTS tc \
                 ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA \
                 AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
                 WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? \
                 AND tc.CONSTRAINT_TYPE = 'CHECK' \
                 ORDER BY cc.CONSTRAINT_NAME",
                &name_params,
            )
            .await
        {
            Ok(set) => set
                .rows
                .iter()
                .filter_map(|row| {
                    Some(CheckConstraintSpec {
                        name: cell_str(row.first())?,
                        definition: cell_str(row.get(1))?,
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        fill_enum_values_from_checks(&mut columns, &check_constraints);

        let incoming_set = self
            .run_query(
                "SELECT CONSTRAINT_NAME, TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, \
                 REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE REFERENCED_TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME = ? \
                 ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
                &name_params,
            )
            .await?;
        let incoming_rows: Vec<IncomingFkColumnRow> = incoming_set
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
        let incoming_foreign_keys = group_incoming_foreign_keys(&incoming_rows);

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
        sqlly_engines::mysql::execute_params_on_client(self.client, sql, params).await
    }

    async fn begin(&mut self) -> StudioResult<()> {
        sqlly_engines::mysql::execute_batch_on_client(self.client, "START TRANSACTION").await
    }

    async fn commit(&mut self) -> StudioResult<()> {
        sqlly_engines::mysql::execute_batch_on_client(self.client, "COMMIT").await
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        sqlly_engines::mysql::execute_batch_on_client(self.client, "ROLLBACK").await
    }

    async fn insert_returning(
        &mut self,
        request: &InsertRowRequest,
    ) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
        let (sql, params) = dialect::build_insert_plain_sql_for(self.db, request);
        let affected = self.execute(&sql, &params).await?;
        if affected != 1 {
            return Err(crate::row_editing::errors::insert_affected_rows(affected));
        }
        let facts = self.key_facts(&request.target).await?;
        match insert_refetch_plan(&facts, &request.values) {
            InsertRefetch::ByAutoIncrement(auto) => {
                let fetch = format!(
                    "SELECT * FROM {} WHERE {} = LAST_INSERT_ID()",
                    dialect::quote_table(self.db, &request.target),
                    self.db.quote_identifier(&auto)
                );
                let set = self.run_query(&fetch, &[]).await?;
                single_row_or_echo(set, request)
            }
            InsertRefetch::ByKey(key) => {
                let (fetch, params) =
                    dialect::build_select_row_sql_for(self.db, &request.target, &key);
                let set = self.run_query(&fetch, &params).await?;
                single_row_or_echo(set, request)
            }
            InsertRefetch::Echo => Ok(echo_values(request)),
        }
    }
}

/// The inserted row when the refetch found exactly one; the supplied values
/// otherwise (the insert itself already succeeded).
fn single_row_or_echo(
    set: MaterializedResultSet,
    request: &InsertRowRequest,
) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
    if set.rows.len() == 1 {
        let columns = set.columns.iter().map(|c| c.name.clone()).collect();
        let row = set.rows.into_iter().next().expect("length checked");
        Ok((columns, row))
    } else {
        Ok(echo_values(request))
    }
}

fn echo_values(request: &InsertRowRequest) -> (Vec<String>, Vec<CellValue>) {
    let columns = request
        .values
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let row = request
        .values
        .iter()
        .map(|(_, value)| value.clone())
        .collect();
    (columns, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, primary: bool, auto: bool) -> MySqlKeyColumn {
        MySqlKeyColumn {
            name: name.to_string(),
            is_primary: primary,
            is_auto_increment: auto,
        }
    }

    #[test]
    fn refetch_prefers_last_insert_id_when_auto_key_left_unset() {
        let columns = vec![key("id", true, true), key("name", false, false)];
        let values = vec![("name".to_string(), CellValue::String("x".into()))];
        assert_eq!(
            insert_refetch_plan(&columns, &values),
            InsertRefetch::ByAutoIncrement("id".into())
        );
        // Explicit NULL also lets the server assign.
        let values = vec![("id".to_string(), CellValue::Null)];
        assert_eq!(
            insert_refetch_plan(&columns, &values),
            InsertRefetch::ByAutoIncrement("id".into())
        );
    }

    #[test]
    fn refetch_uses_supplied_key_when_auto_value_was_given() {
        let columns = vec![key("id", true, true)];
        let values = vec![("id".to_string(), CellValue::I64(7))];
        assert_eq!(
            insert_refetch_plan(&columns, &values),
            InsertRefetch::ByKey(vec![("id".into(), CellValue::I64(7))])
        );
    }

    #[test]
    fn refetch_uses_full_composite_key_or_echoes() {
        let columns = vec![key("a", true, false), key("b", true, false)];
        let complete = vec![
            ("a".to_string(), CellValue::I64(1)),
            ("b".to_string(), CellValue::I64(2)),
        ];
        assert_eq!(
            insert_refetch_plan(&columns, &complete),
            InsertRefetch::ByKey(complete.clone())
        );
        let partial = vec![("a".to_string(), CellValue::I64(1))];
        assert_eq!(insert_refetch_plan(&columns, &partial), InsertRefetch::Echo);
    }

    #[test]
    fn refetch_echoes_without_any_primary_key() {
        let columns = vec![key("x", false, false)];
        let values = vec![("x".to_string(), CellValue::I64(1))];
        assert_eq!(insert_refetch_plan(&columns, &values), InsertRefetch::Echo);
    }

    fn live_mysql_profile() -> Option<sqlly_core::ConnectionProfile> {
        use sqlly_core::{ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
        let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
        let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
        let database = std::env::var("SQLLY_MYSQL_DATABASE").ok()?;
        let mut profile = ConnectionProfile::sql_login(
            "mysql-rowedit-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_MYSQL_PASSWORD".into(),
            },
        );
        profile.port = std::env::var("SQLLY_MYSQL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3306);
        profile.database = Some(database);
        profile.database_type = DatabaseType::MySql;
        profile.encrypt = false;
        Some(profile)
    }

    /// Live lifecycle test: set SQLLY_MYSQL_SERVER / SQLLY_MYSQL_USER /
    /// SQLLY_MYSQL_PASSWORD / SQLLY_MYSQL_DATABASE (and optionally
    /// SQLLY_MYSQL_PORT). Skips silently when no live server is configured.
    #[tokio::test]
    async fn live_mysql_row_edit_lifecycle() {
        use crate::row_editing::{
            DefaultRowEditingService, DeleteRowOutcome, DeleteRowRequest, InsertRowOutcome,
            InsertRowRequest, RowEditOpenOutcome, RowEditOpenRequest, RowEditingService,
            UpdateRowOutcome, UpdateRowRequest,
        };

        let Some(profile) = live_mysql_profile() else {
            eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
            return;
        };

        let client = sqlly_engines::mysql::open_client(&profile).await.unwrap();
        for statement in [
            "DROP TABLE IF EXISTS sqlly_rowedit_live",
            "CREATE TABLE sqlly_rowedit_live (\
             id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL, \
             price DECIMAL(10,2), flag TINYINT(1))",
            "INSERT INTO sqlly_rowedit_live (name, price, flag) \
             VALUES ('Ada', 1.50, 1), ('Grace', 2.25, 0)",
        ] {
            sqlly_engines::mysql::execute_batch_on_client(&client, statement)
                .await
                .unwrap();
        }

        let service = DefaultRowEditingService::default();
        let sql = "SELECT id, name, price FROM sqlly_rowedit_live ORDER BY id";
        let sets = sqlly_engines::mysql::query_rows_on_client(&client, sql, &[])
            .await
            .unwrap();
        let set = sets.into_iter().next().unwrap();
        let request = RowEditOpenRequest {
            sql: sql.to_string(),
            result_set_index: 0,
            columns: set.columns,
            row: set.rows[0].clone(),
        };

        // Open: provenance from the protocol's original-table column
        // metadata; identity and tinyint(1) classification surface.
        let session = match service.open_session(&profile, request).await.unwrap() {
            RowEditOpenOutcome::Editable(session) => *session,
            RowEditOpenOutcome::NotEditable { reason, .. } => panic!("not editable: {reason}"),
        };
        assert_eq!(session.target.table, "sqlly_rowedit_live");
        assert_eq!(session.key_columns, vec!["id"]);
        assert!(session.columns[0].is_identity);
        assert_eq!(session.columns[3].data_type, "tinyint(1)");

        // Guarded update; CLIENT_FOUND_ROWS keeps a value-identical update
        // from being misread as a conflict.
        let update = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Ada".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        assert!(matches!(
            service.update_row(&profile, update).await.unwrap(),
            UpdateRowOutcome::Updated { .. }
        ));
        // A genuinely stale WHERE still conflicts.
        let stale = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Bob".into()))],
            original_values: vec![("name".into(), CellValue::String("NotAda".into()))],
            expressions: Vec::new(),
        };
        assert!(matches!(
            service.update_row(&profile, stale).await.unwrap(),
            UpdateRowOutcome::Conflict { .. }
        ));

        // Insert with the auto-increment key omitted: refetched through
        // LAST_INSERT_ID inside the transaction.
        let inserted = service
            .insert_row(
                &profile,
                InsertRowRequest {
                    target: session.target.clone(),
                    values: vec![
                        ("name".into(), CellValue::String("Zed".into())),
                        ("price".into(), CellValue::String("3.00".into())),
                    ],
                    expressions: Vec::new(),
                },
            )
            .await
            .unwrap();
        let InsertRowOutcome::Inserted { columns, row } = inserted;
        let id_at = columns.iter().position(|c| c == "id").unwrap();
        let CellValue::I64(new_id) = row[id_at] else {
            panic!("expected integer id");
        };
        assert!(new_id > 2, "auto-increment resolved");

        let delete = DeleteRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(new_id))],
            rowversion: None,
            original_values: vec![("name".into(), CellValue::String("Zed".into()))],
        };
        assert!(matches!(
            service.delete_row(&profile, delete.clone()).await.unwrap(),
            DeleteRowOutcome::Deleted
        ));
        assert!(matches!(
            service.delete_row(&profile, delete).await.unwrap(),
            DeleteRowOutcome::Conflict { .. }
        ));

        sqlly_engines::mysql::execute_batch_on_client(&client, "DROP TABLE sqlly_rowedit_live")
            .await
            .unwrap();
    }
}
