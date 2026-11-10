//! Follow a foreign-key cell: resolve provenance via the row-editing
//! service, then open a new query tab on the **current** connection with a
//! single-row SELECT against the referenced table.

use super::*;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::row_editing::{ForeignKeySpec, RowEditOpenOutcome, RowEditSession};

use crate::sql_literal::{sql_identifier_for, sql_literal_cell_for};
use crate::table_actions::QualifiedTable;

#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

impl AppView {
    pub(super) fn follow_foreign_key(
        &mut self,
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        column_index: usize,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.foreign_keys.connect_first"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        let column_name = columns
            .get(column_index)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        self.with_entra_sql_token(profile, cx, move |this, profile, cx| {
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns: columns.clone(),
                row: row.clone(),
            };
            let services = this.services_for_profile(&profile);
            let db = profile.database_type;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(async move {
                        services
                            .row_editing
                            .open_insert_session(&profile, request)
                            .await
                    })
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(RowEditOpenOutcome::Editable(session)) => {
                                match fk_for_column(&session, &column_name) {
                                    Some(fk) => match referenced_row_sql(db, fk, &columns, &row) {
                                        Some(follow_sql) => {
                                            let schema = fk.referenced.schema.clone();
                                            let table = fk.referenced.table.clone();
                                            this.new_tab(cx, Some(follow_sql));
                                            let idx = this.active_tab;
                                            this.execute_query(idx, cx);
                                            this.set_status(
                                                StatusLevel::Info,
                                                crate::localization::format(
                                                    "shell.foreign_keys.opened",
                                                    &[("schema", &schema), ("table", &table)],
                                                ),
                                            );
                                        }
                                        None => {
                                            this.set_status(
                                                StatusLevel::Info,
                                                crate::localization::text(
                                                    "shell.foreign_keys.null_value",
                                                ),
                                            );
                                        }
                                    },
                                    None => {
                                        this.set_status(
                                            StatusLevel::Info,
                                            crate::localization::format(
                                                "shell.foreign_keys.not_foreign_key",
                                                &[("column", &column_name)],
                                            ),
                                        );
                                    }
                                }
                            }
                            Ok(RowEditOpenOutcome::NotEditable { reason, message }) => {
                                let reason = crate::localization::stored_message_or(
                                    message.as_ref(),
                                    reason,
                                );
                                this.set_status(StatusLevel::Info, reason);
                            }
                            Err(err) => {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::message(err),
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        });
    }
}

/// The FK whose local columns include `column_name` (case-insensitive).
/// Prefers a single-column FK when several match.
pub(crate) fn fk_for_column<'a>(
    session: &'a RowEditSession,
    column_name: &str,
) -> Option<&'a ForeignKeySpec> {
    let matches: Vec<&ForeignKeySpec> = session
        .foreign_keys
        .iter()
        .filter(|fk| {
            fk.column_pairs
                .iter()
                .any(|(local, _)| local.eq_ignore_ascii_case(column_name))
        })
        .collect();
    matches
        .iter()
        .copied()
        .find(|fk| fk.column_pairs.len() == 1)
        .or_else(|| matches.first().copied())
}

/// `SELECT … LIMIT 1` (engine-specific) against the referenced table, keyed
/// by every `(local, referenced)` pair's value from `row`. Returns `None` if
/// any local value is NULL or a pair cannot be resolved.
pub(crate) fn referenced_row_sql(
    db: DatabaseType,
    fk: &ForeignKeySpec,
    columns: &[ResultColumn],
    row: &[CellValue],
) -> Option<String> {
    let mut predicates = Vec::new();
    for (local, referenced) in &fk.column_pairs {
        let i = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(local))?;
        let cell = row.get(i)?;
        let literal = sql_literal_cell_for(db, cell)?;
        let ident = sql_identifier_for(db, referenced);
        predicates.push(format!("{ident} = {literal}"));
    }
    if predicates.is_empty() {
        return None;
    }
    let qt = QualifiedTable::new(&fk.referenced.table).with_schema(&fk.referenced.schema);
    // The WHERE sits between the halves of the engine's limiter: `TOP 1`
    // leads, `LIMIT 1` / `FETCH FIRST` trail it.
    let (limit_prefix, limit_suffix) = db.row_limit_clauses(1);
    let mut sql = format!(
        "SELECT {limit_prefix}*\nFROM {}\nWHERE {}",
        qt.schema_qualified_table_for(db),
        predicates.join(" AND ")
    );
    if !limit_suffix.is_empty() {
        sql.push('\n');
        sql.push_str(&limit_suffix);
    }
    Some(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::row_editing::TableTarget;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: true,
        }
    }

    fn fk(constraint: &str, schema: &str, table: &str, pairs: Vec<(&str, &str)>) -> ForeignKeySpec {
        ForeignKeySpec {
            constraint_name: constraint.into(),
            referenced: TableTarget {
                schema: schema.into(),
                table: table.into(),
            },
            column_pairs: pairs
                .into_iter()
                .map(|(l, r)| (l.into(), r.into()))
                .collect(),
            display_column: None,
        }
    }

    fn session(foreign_keys: Vec<ForeignKeySpec>) -> RowEditSession {
        let fk_display = vec![None; foreign_keys.len()];
        RowEditSession {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            key_columns: vec![],
            rowversion_column: None,
            columns: vec![],
            original_row: vec![],
            foreign_keys,
            fk_display,
            check_constraints: vec![],
            incoming_foreign_keys: vec![],
        }
    }

    #[test]
    fn single_column_fk_sql_server_and_postgres() {
        let spec = fk(
            "FK_Orders_Customer",
            "dbo",
            "Customer",
            vec![("CustomerId", "Id")],
        );
        let columns = vec![col("Id"), col("CustomerId")];
        let row = vec![CellValue::I64(1), CellValue::I64(42)];
        let sql_server =
            referenced_row_sql(DatabaseType::SqlServer, &spec, &columns, &row).expect("sql");
        let expected = DatabaseType::SqlServer.render_limited_select(
            "*",
            &QualifiedTable::new("Customer")
                .with_schema("dbo")
                .schema_qualified_table_for(DatabaseType::SqlServer),
            1,
        );
        assert_eq!(sql_server, format!("{expected}\nWHERE [Id] = 42"));
        // LIMIT engines: the WHERE goes before the limiter, or the
        // statement is a syntax error.
        let pg = referenced_row_sql(DatabaseType::Postgres, &spec, &columns, &row).expect("sql");
        assert_eq!(
            pg,
            "SELECT *\nFROM \"dbo\".\"Customer\"\nWHERE \"Id\" = 42\nLIMIT 1"
        );
        let oracle = referenced_row_sql(DatabaseType::Oracle, &spec, &columns, &row).expect("sql");
        assert!(
            oracle.ends_with("WHERE \"Id\" = 42\nFETCH FIRST 1 ROWS ONLY"),
            "{oracle}"
        );
    }

    #[test]
    fn composite_fk_joins_predicates_with_and() {
        let spec = fk("FK_Line", "dbo", "Parent", vec![("A", "Pa"), ("B", "Pb")]);
        let columns = vec![col("A"), col("B")];
        let row = vec![CellValue::I64(1), CellValue::I64(2)];
        let sql = referenced_row_sql(DatabaseType::SqlServer, &spec, &columns, &row).unwrap();
        assert!(sql.contains("[Pa] = 1 AND [Pb] = 2"), "{sql}");
    }

    /// Sequel Ace #2361: following a binary UUID foreign key must key the
    /// referenced row by the real bytes, not refuse it as if it were NULL.
    #[test]
    fn binary_fk_value_keys_the_referenced_row_by_its_bytes() {
        let spec = fk("FK_Order_User", "app", "users", vec![("user_id", "id")]);
        let columns = vec![col("user_id")];
        let row = vec![CellValue::Bytes(vec![0x11, 0xEE, 0x0A])];
        let mysql = referenced_row_sql(DatabaseType::MySql, &spec, &columns, &row).expect("sql");
        assert!(mysql.ends_with("WHERE `id` = 0x11EE0A\nLIMIT 1"), "{mysql}");
        let pg = referenced_row_sql(DatabaseType::Postgres, &spec, &columns, &row).expect("sql");
        assert!(pg.ends_with("WHERE \"id\" = '\\x11EE0A'\nLIMIT 1"), "{pg}");
        let sqlite = referenced_row_sql(DatabaseType::Sqlite, &spec, &columns, &row).expect("sql");
        assert!(
            sqlite.ends_with("WHERE \"id\" = X'11EE0A'\nLIMIT 1"),
            "{sqlite}"
        );
    }

    #[test]
    fn null_local_value_returns_none() {
        let spec = fk("FK", "dbo", "Customer", vec![("CustomerId", "Id")]);
        let columns = vec![col("CustomerId")];
        let row = vec![CellValue::Null];
        assert!(referenced_row_sql(DatabaseType::SqlServer, &spec, &columns, &row).is_none());
    }

    #[test]
    fn fk_for_column_is_case_insensitive_and_prefers_single_column() {
        let composite = fk(
            "FK_comp",
            "dbo",
            "Parent",
            vec![("CustomerId", "Id"), ("OrgId", "Org")],
        );
        let single = fk("FK_single", "dbo", "Customer", vec![("CustomerId", "Id")]);
        let session = session(vec![composite, single.clone()]);
        let found = fk_for_column(&session, "customerid").expect("fk");
        assert_eq!(found.constraint_name, "FK_single");
        assert_eq!(found.column_pairs.len(), 1);
    }
}
