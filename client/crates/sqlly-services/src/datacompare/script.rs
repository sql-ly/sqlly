//! Review-only INSERT/UPDATE/DELETE sync scripts from a [`DataDiff`].

use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::datacompare::DataDiff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    LeftToRight,
    RightToLeft,
}

pub struct SyncScriptRequest<'a> {
    pub db: DatabaseType,
    pub destination_table: &'a str,
    pub columns: &'a [ResultColumn],
    pub key_indexes: &'a [usize],
    pub diff: &'a DataDiff,
    pub direction: SyncDirection,
    pub header_note: &'a str,
}

pub struct SyncScript {
    pub sql: String,
    pub inserts: usize,
    pub updates: usize,
    pub deletes: usize,
    pub skipped: usize,
}

pub fn literal(db: DatabaseType, cell: &CellValue) -> Option<String> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(db.bool_literal(*b).to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::String(s) => Some(db.string_literal(s)),
        CellValue::Bytes(_) => None,
    }
}

pub fn generate_sync_script(req: &SyncScriptRequest<'_>) -> SyncScript {
    let mut statements = Vec::new();
    let mut inserts = 0;
    let mut updates = 0;
    let mut deletes = 0;
    let mut skipped = 0;

    let (insert_rows, delete_rows, update_pairs) = match req.direction {
        SyncDirection::LeftToRight => {
            (&req.diff.only_left, &req.diff.only_right, &req.diff.changed)
        }
        SyncDirection::RightToLeft => {
            (&req.diff.only_right, &req.diff.only_left, &req.diff.changed)
        }
    };

    for row in insert_rows {
        match insert_statement(req, row) {
            StatementOutcome::Sql(sql) => {
                inserts += 1;
                statements.push(sql);
            }
            StatementOutcome::Skipped(comment) => {
                skipped += 1;
                statements.push(comment);
            }
        }
    }

    for row in delete_rows {
        match delete_statement(req, row) {
            StatementOutcome::Sql(sql) => {
                deletes += 1;
                statements.push(sql);
            }
            StatementOutcome::Skipped(comment) => {
                skipped += 1;
                statements.push(comment);
            }
        }
    }

    for pair in update_pairs {
        match update_statement(req, pair) {
            StatementOutcome::Sql(sql) => {
                updates += 1;
                statements.push(sql);
            }
            StatementOutcome::Skipped(comment) => {
                skipped += 1;
                statements.push(comment);
            }
        }
    }

    let (begin, commit) = transaction_wrappers(req.db);
    let mut sql = format!(
        "-- Data compare sync: {}\n\
         -- Direction: {}. Review carefully; nothing runs until you press Run.\n\
         -- {inserts} insert(s), {updates} update(s), {deletes} delete(s), {skipped} skipped\n",
        req.header_note,
        direction_label(req.direction)
    );
    if let Some(note) = &req.diff.truncated {
        sql.push_str(&format!(
            "-- WARNING: the compare was truncated ({note}); this script is incomplete.\n"
        ));
    }
    if statements.is_empty() {
        sql.push_str("-- Nothing to sync.\n");
    } else {
        if !begin.is_empty() {
            sql.push_str(&format!("{begin}\n"));
        }
        sql.push_str(&statements.join("\n"));
        sql.push('\n');
        sql.push_str(&format!("{commit}\n"));
    }

    SyncScript {
        sql,
        inserts,
        updates,
        deletes,
        skipped,
    }
}

enum StatementOutcome {
    Sql(String),
    Skipped(String),
}

fn direction_label(direction: SyncDirection) -> &'static str {
    match direction {
        SyncDirection::LeftToRight => "left -> right",
        SyncDirection::RightToLeft => "right -> left",
    }
}

fn transaction_wrappers(db: DatabaseType) -> (&'static str, &'static str) {
    use DatabaseType::*;
    match db {
        SqlServer | AzureSqlDatabase => ("BEGIN TRANSACTION;", "COMMIT;"),
        MySql | MariaDb => ("START TRANSACTION;", "COMMIT;"),
        ClickHouse => (
            "-- ClickHouse has no transactions: each statement below is applied on its own.",
            "-- Deletes and updates are asynchronous mutations; check system.mutations for progress.",
        ),
        // Oracle has no BEGIN: the session's transaction is already open,
        // so only the closing COMMIT is scripted.
        Oracle => ("", "COMMIT;"),
        _ => ("BEGIN;", "COMMIT;"),
    }
}

/// ClickHouse has no row-level UPDATE statement; changes are `ALTER TABLE …
/// UPDATE` mutations. Deletes use the standard `DELETE FROM … WHERE`
/// (lightweight delete).
fn is_clickhouse(db: DatabaseType) -> bool {
    matches!(
        db.connection_kind(),
        sqlly_lang::sqlmodel::ConnectionKind::ClickHouseHttp
    )
}

fn quote(db: DatabaseType, name: &str) -> String {
    db.quote_identifier(name)
}

fn render_cell(db: DatabaseType, cell: &CellValue) -> Result<String, String> {
    if matches!(cell, CellValue::Bytes(_)) {
        return Err("binary".into());
    }
    match cell {
        CellValue::Null => Ok("NULL".to_string()),
        other => literal(db, other).ok_or_else(|| "unsupported".into()),
    }
}

fn key_text(req: &SyncScriptRequest<'_>, row: &[CellValue]) -> String {
    req.key_indexes
        .iter()
        .filter_map(|index| row.get(*index))
        .map(typed_cell_text)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn typed_cell_text(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => "NULL".into(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!(
            "0x{}",
            b.iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
    }
}

fn insert_statement(req: &SyncScriptRequest<'_>, row: &[CellValue]) -> StatementOutcome {
    let column_names = req
        .columns
        .iter()
        .map(|column| quote(req.db, &column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = Vec::with_capacity(row.len());
    for (index, cell) in row.iter().enumerate() {
        match render_cell(req.db, cell) {
            Ok(value) => values.push(value),
            Err(_) => {
                let column = req
                    .columns
                    .get(index)
                    .map(|c| c.name.as_str())
                    .unwrap_or("?");
                return StatementOutcome::Skipped(format!(
                    "-- skipped (binary value in column {column}): key = {}",
                    key_text(req, row)
                ));
            }
        }
    }
    StatementOutcome::Sql(format!(
        "INSERT INTO {} ({column_names}) VALUES ({});",
        req.destination_table,
        values.join(", ")
    ))
}

fn delete_statement(req: &SyncScriptRequest<'_>, row: &[CellValue]) -> StatementOutcome {
    match where_clause(req, row) {
        Ok(where_clause) => StatementOutcome::Sql(format!(
            "DELETE FROM {} WHERE {where_clause};",
            req.destination_table
        )),
        Err(column) => StatementOutcome::Skipped(format!(
            "-- skipped (binary value in column {column}): key = {}",
            key_text(req, row)
        )),
    }
}

fn update_statement(
    req: &SyncScriptRequest<'_>,
    pair: &crate::datacompare::ChangedPair,
) -> StatementOutcome {
    let source = match req.direction {
        SyncDirection::LeftToRight => &pair.left,
        SyncDirection::RightToLeft => &pair.right,
    };
    let assignments = pair
        .changed_columns
        .iter()
        .filter_map(|index| {
            let column = req.columns.get(*index)?;
            let cell = source.get(*index)?;
            match render_cell(req.db, cell) {
                Ok(value) => Some(Ok(format!("{} = {value}", quote(req.db, &column.name)))),
                Err(_) => Some(Err(column.name.clone())),
            }
        })
        .collect::<Result<Vec<_>, _>>();
    let assignments = match assignments {
        Ok(values) if values.is_empty() => {
            return StatementOutcome::Skipped(format!(
                "-- skipped (no writable columns): key = {}",
                key_text(req, source)
            ));
        }
        Ok(values) => values.join(", "),
        Err(column) => {
            return StatementOutcome::Skipped(format!(
                "-- skipped (binary value in column {column}): key = {}",
                key_text(req, source)
            ));
        }
    };
    match where_clause(req, source) {
        Ok(where_clause) if is_clickhouse(req.db) => StatementOutcome::Sql(format!(
            "ALTER TABLE {} UPDATE {assignments} WHERE {where_clause};",
            req.destination_table
        )),
        Ok(where_clause) => StatementOutcome::Sql(format!(
            "UPDATE {} SET {assignments} WHERE {where_clause};",
            req.destination_table
        )),
        Err(column) => StatementOutcome::Skipped(format!(
            "-- skipped (binary value in column {column}): key = {}",
            key_text(req, source)
        )),
    }
}

fn where_clause(req: &SyncScriptRequest<'_>, row: &[CellValue]) -> Result<String, String> {
    let mut parts = Vec::new();
    for index in req.key_indexes {
        let column = &req.columns[*index];
        let cell = row.get(*index).ok_or_else(|| column.name.clone())?;
        let value = render_cell(req.db, cell).map_err(|_| column.name.clone())?;
        parts.push(format!("{} = {value}", quote(req.db, &column.name)));
    }
    Ok(parts.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datacompare::{ChangedPair, DataDiff};

    fn fixture_columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
            ResultColumn {
                name: "active".into(),
                data_type: "bit".into(),
                nullable: true,
            },
        ]
    }

    fn fixture_diff() -> DataDiff {
        DataDiff {
            only_left: vec![vec![
                CellValue::I64(1),
                CellValue::String("O'Brien".into()),
                CellValue::Bool(true),
            ]],
            only_right: vec![vec![
                CellValue::I64(2),
                CellValue::String("x".into()),
                CellValue::Bool(false),
            ]],
            changed: vec![ChangedPair {
                key: vec![CellValue::I64(3)],
                left: vec![
                    CellValue::I64(3),
                    CellValue::String("a".into()),
                    CellValue::Bool(true),
                ],
                right: vec![
                    CellValue::I64(3),
                    CellValue::String("b".into()),
                    CellValue::Bool(true),
                ],
                changed_columns: vec![1],
            }],
            unchanged: 0,
            scanned_left: 0,
            scanned_right: 0,
            truncated: None,
        }
    }

    fn request<'a>(
        db: DatabaseType,
        table: &'a str,
        columns: &'a [ResultColumn],
        diff: &'a DataDiff,
        direction: SyncDirection,
    ) -> SyncScriptRequest<'a> {
        SyncScriptRequest {
            db,
            destination_table: table,
            columns,
            key_indexes: &[0],
            diff,
            direction,
            header_note: "test",
        }
    }

    #[test]
    fn sql_server_left_to_right_golden() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let script = generate_sync_script(&request(
            DatabaseType::SqlServer,
            "[dbo].[t]",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        let expected = "\
-- Data compare sync: test
-- Direction: left -> right. Review carefully; nothing runs until you press Run.
-- 1 insert(s), 1 update(s), 1 delete(s), 0 skipped
BEGIN TRANSACTION;
INSERT INTO [dbo].[t] ([id], [name], [active]) VALUES (1, N'O''Brien', 1);
DELETE FROM [dbo].[t] WHERE [id] = 2;
UPDATE [dbo].[t] SET [name] = N'a' WHERE [id] = 3;
COMMIT;
";
        assert_eq!(script.sql, expected);
    }

    #[test]
    fn postgres_left_to_right_golden() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let script = generate_sync_script(&request(
            DatabaseType::Postgres,
            "\"public\".\"t\"",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert!(script.sql.contains("BEGIN;\n"));
        assert!(script.sql.contains(
            "INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"active\") VALUES (1, 'O''Brien', TRUE);"
        ));
        assert!(
            script
                .sql
                .contains("UPDATE \"public\".\"t\" SET \"name\" = 'a' WHERE \"id\" = 3;")
        );
        assert!(
            script
                .sql
                .contains("DELETE FROM \"public\".\"t\" WHERE \"id\" = 2;")
        );
        assert!(script.sql.contains("COMMIT;\n"));
    }

    /// Oracle has no `BEGIN`; the script opens straight into the statements
    /// (no blank line where an opener would be) and closes with COMMIT.
    #[test]
    fn oracle_scripts_no_begin_and_a_closing_commit() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let script = generate_sync_script(&request(
            DatabaseType::Oracle,
            "\"SQLLY\".\"T\"",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert!(!script.sql.contains("BEGIN"));
        assert!(!script.sql.contains("\n\n"));
        assert!(script.sql.contains(
            "INSERT INTO \"SQLLY\".\"T\" (\"id\", \"name\", \"active\") VALUES (1, 'O''Brien', 1);"
        ));
        assert!(script.sql.ends_with("COMMIT;\n"));
    }

    #[test]
    fn clickhouse_updates_are_mutations_and_nothing_is_wrapped_in_a_transaction() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let req = request(
            DatabaseType::ClickHouse,
            "`shop`.`t`",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        );
        let script = generate_sync_script(&req);
        assert_eq!((script.inserts, script.updates, script.deletes), (1, 1, 1));
        assert!(
            script.sql.contains("ClickHouse has no transactions"),
            "{}",
            script.sql
        );
        assert!(!script.sql.contains("BEGIN;"), "{}", script.sql);
        assert!(!script.sql.contains("COMMIT;"), "{}", script.sql);
        assert!(
            script.sql.contains(
                "INSERT INTO `shop`.`t` (`id`, `name`, `active`) VALUES (1, 'O''Brien', TRUE);"
            ),
            "{}",
            script.sql
        );
        assert!(
            script
                .sql
                .contains("DELETE FROM `shop`.`t` WHERE `id` = 2;"),
            "{}",
            script.sql
        );
        assert!(
            script
                .sql
                .contains("ALTER TABLE `shop`.`t` UPDATE `name` = 'a' WHERE `id` = 3;"),
            "{}",
            script.sql
        );
        assert!(
            !script.sql.contains("UPDATE `shop`.`t` SET"),
            "{}",
            script.sql
        );
    }

    #[test]
    fn mysql_left_to_right_golden() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let db = DatabaseType::MySql;
        let script = generate_sync_script(&request(
            db,
            "`t`",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        let true_lit = db.bool_literal(true);
        let false_lit = db.bool_literal(false);
        assert!(script.sql.contains("START TRANSACTION;\n"));
        assert!(script.sql.contains(&format!(
            "INSERT INTO `t` (`id`, `name`, `active`) VALUES (1, 'O''Brien', {true_lit});"
        )));
        assert!(script.sql.contains("DELETE FROM `t` WHERE `id` = 2;"));
        assert!(
            script
                .sql
                .contains("UPDATE `t` SET `name` = 'a' WHERE `id` = 3;")
        );
        assert!(script.sql.contains(false_lit));
    }

    #[test]
    fn sqlite_left_to_right_golden() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let db = DatabaseType::Sqlite;
        let script = generate_sync_script(&request(
            db,
            "\"t\"",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert!(script.sql.contains("BEGIN;\n"));
        assert!(script.sql.contains(&format!(
            "INSERT INTO \"t\" (\"id\", \"name\", \"active\") VALUES (1, 'O''Brien', {});",
            db.bool_literal(true)
        )));
    }

    #[test]
    fn right_to_left_mirrors_insert_delete_sets() {
        let columns = fixture_columns();
        let diff = fixture_diff();
        let script = generate_sync_script(&request(
            DatabaseType::SqlServer,
            "[dbo].[t]",
            &columns,
            &diff,
            SyncDirection::RightToLeft,
        ));
        assert!(
            script
                .sql
                .contains("INSERT INTO [dbo].[t] ([id], [name], [active]) VALUES (2, N'x', 0);")
        );
        assert!(script.sql.contains("DELETE FROM [dbo].[t] WHERE [id] = 1;"));
        assert!(
            script
                .sql
                .contains("UPDATE [dbo].[t] SET [name] = N'b' WHERE [id] = 3;")
        );
    }

    #[test]
    fn bytes_row_is_skipped() {
        let columns = fixture_columns();
        let diff = DataDiff {
            only_left: vec![vec![
                CellValue::I64(9),
                CellValue::Bytes(vec![1, 2]),
                CellValue::Bool(false),
            ]],
            ..DataDiff::default()
        };
        let script = generate_sync_script(&request(
            DatabaseType::SqlServer,
            "[dbo].[t]",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert_eq!(script.skipped, 1);
        assert!(
            script
                .sql
                .contains("-- skipped (binary value in column name)")
        );
    }

    #[test]
    fn truncated_diff_adds_warning() {
        let columns = fixture_columns();
        let mut diff = fixture_diff();
        diff.truncated = Some("cap hit".into());
        let script = generate_sync_script(&request(
            DatabaseType::SqlServer,
            "[dbo].[t]",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert!(script.sql.contains(
            "-- WARNING: the compare was truncated (cap hit); this script is incomplete."
        ));
    }

    #[test]
    fn empty_diff_emits_nothing_to_sync() {
        let columns = fixture_columns();
        let diff = DataDiff::default();
        let script = generate_sync_script(&request(
            DatabaseType::SqlServer,
            "[dbo].[t]",
            &columns,
            &diff,
            SyncDirection::LeftToRight,
        ));
        assert!(script.sql.contains("-- Nothing to sync."));
        assert!(!script.sql.contains("BEGIN TRANSACTION;"));
    }
}
