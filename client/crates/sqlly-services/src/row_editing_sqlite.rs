//! SQLite row-editing engine.
//!
//! Provenance comes from the engine's own description of the prepared
//! statement: SQLite's `sqlite3_column_{database,table,origin}_name` report
//! the attached database, base table, and base column each projected column
//! derives from — the SQLite equivalent of `sp_describe_first_result_set`.
//! Table metadata comes from the pragma table-valued functions
//! (`pragma_table_list`, `pragma_table_xinfo`, `pragma_foreign_key_list`),
//! all fully parameterized.
//!
//! Primary keys come from `table_xinfo`'s `pk` ordinal. A single-column
//! `INTEGER PRIMARY KEY` on a rowid table is the rowid alias — the engine
//! assigns it when omitted — so it is surfaced as an identity column.
//! Generated columns (`hidden` 2/3 in `table_xinfo`) are read-only computed
//! columns. Transactions open `BEGIN IMMEDIATE` so the write lock is taken up
//! front rather than failing on upgrade mid-operation.

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioResult};

use crate::row_editing::{
    DescribeColumn, ForeignKeySpec, IncomingForeignKey, LoadedTable, RowEditColumn, TableTarget,
};
use crate::row_editing_engines::RowEditEngine;
use crate::row_editing_sql as dialect;

/// A parsed SQLite column declaration: `VARCHAR(30)` → base `varchar` with a
/// character length; `DECIMAL(10,2)` → base `decimal` with precision/scale.
/// SQLite does not enforce any of it, but the editor uses it for gentle
/// client-side validation, exactly like declared lengths elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqliteDecl {
    pub base: String,
    pub char_len: Option<i64>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
}

/// Splits a declared type into its lowercased base name and any
/// parenthesized numbers, mapped onto length or precision/scale by family.
pub(crate) fn parse_decl(decl: &str) -> SqliteDecl {
    let trimmed = decl.trim();
    let (base, args) = match trimmed.split_once('(') {
        Some((base, rest)) => {
            let args: Vec<i64> = rest
                .trim_end_matches(')')
                .split(',')
                .filter_map(|n| n.trim().parse::<i64>().ok())
                .collect();
            (base.trim().to_lowercase(), args)
        }
        None => (trimmed.to_lowercase(), Vec::new()),
    };
    let is_char = base.contains("char") || base.contains("text") || base.contains("clob");
    let is_decimal = matches!(base.as_str(), "decimal" | "numeric");
    SqliteDecl {
        char_len: if is_char { args.first().copied() } else { None },
        precision: if is_decimal {
            args.first().and_then(|p| u8::try_from(*p).ok())
        } else {
            None
        },
        scale: if is_decimal {
            args.get(1).and_then(|s| u8::try_from(*s).ok())
        } else {
            None
        },
        base,
    }
}

/// Whether a single-column primary key on a rowid table is the rowid alias
/// (declared type exactly `INTEGER`, any case) — the engine auto-assigns it
/// when omitted, so the editor treats it like an identity column.
pub(crate) fn is_rowid_alias(
    key_columns: &[String],
    declared: &[(String, String)],
    without_rowid: bool,
) -> Option<String> {
    if without_rowid || key_columns.len() != 1 {
        return None;
    }
    let key = &key_columns[0];
    declared
        .iter()
        .find(|(name, _)| name == key)
        .filter(|(_, decl)| decl.trim().eq_ignore_ascii_case("integer"))
        .map(|(name, _)| name.clone())
}

pub(crate) struct SqliteEngine<'a> {
    client: &'a sqlly_engines::sqlite::SqliteClient,
    db: DatabaseType,
}

impl<'a> SqliteEngine<'a> {
    pub(crate) fn new(client: &'a sqlly_engines::sqlite::SqliteClient, db: DatabaseType) -> Self {
        Self { client, db }
    }

    async fn run_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let sets = sqlly_engines::sqlite::query_rows_on_client(self.client, sql, params).await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    /// Primary-key column names of a table, in key order.
    async fn table_key_columns(&self, target: &TableTarget) -> StudioResult<Vec<String>> {
        let set = self
            .run_query(
                "SELECT name, pk FROM main.pragma_table_xinfo(?1, ?2) WHERE pk > 0 ORDER BY pk",
                &[
                    SqlParam::String(target.table.clone()),
                    SqlParam::String(target.schema.clone()),
                ],
            )
            .await?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect())
    }

    /// Best-effort display column for a referenced table: a text-ish declared
    /// column with a name-like identifier.
    async fn display_column_for(&self, target: &TableTarget) -> Option<String> {
        let set = self
            .run_query(
                "SELECT name, type FROM main.pragma_table_xinfo(?1, ?2) WHERE hidden = 0",
                &[
                    SqlParam::String(target.table.clone()),
                    SqlParam::String(target.schema.clone()),
                ],
            )
            .await
            .ok()?;
        let candidates = [
            "name",
            "displayname",
            "fullname",
            "title",
            "label",
            "description",
        ];
        let mut best: Option<(usize, String)> = None;
        for row in &set.rows {
            let (Some(name), Some(decl)) = (cell_str(row.first()), cell_str(row.get(1))) else {
                continue;
            };
            let base = parse_decl(&decl).base;
            let textish = base.contains("char") || base.contains("text") || base.contains("clob");
            if !textish {
                continue;
            }
            let lower = name.to_lowercase();
            if let Some(rank) = candidates.iter().position(|c| *c == lower)
                && best.as_ref().is_none_or(|(r, _)| rank < *r)
            {
                best = Some((rank, name));
            }
        }
        best.map(|(_, name)| name)
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
impl RowEditEngine for SqliteEngine<'_> {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, sqlly_core::DisplayMessage>> {
        let origins =
            match sqlly_engines::sqlite::describe_statement_on_client(self.client, sql).await {
                Ok(origins) => origins,
                Err(err) => {
                    return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                        err,
                    )));
                }
            };
        Ok(Ok(origins
            .into_iter()
            .map(|origin| DescribeColumn {
                name: origin.name,
                is_hidden: false,
                source_schema: origin.database,
                source_table: origin.table,
                source_column: origin.column,
            })
            .collect()))
    }

    async fn load_table(
        &mut self,
        target: &TableTarget,
    ) -> StudioResult<Result<LoadedTable, sqlly_core::DisplayMessage>> {
        let quoted = dialect::quote_table(self.db, target);
        let name_params = [
            SqlParam::String(target.table.clone()),
            SqlParam::String(target.schema.clone()),
        ];

        // Existence + base-table + rowid-ness in one pragma.
        let set = self
            .run_query(
                "SELECT type, wr FROM main.pragma_table_list WHERE schema = ?2 AND name = ?1",
                &name_params,
            )
            .await?;
        let Some(row) = set.rows.first() else {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        };
        let table_type = cell_str(row.first()).unwrap_or_default();
        if !table_type.eq_ignore_ascii_case("table") {
            return Ok(Err(crate::row_editing::not_editable::not_base_table(
                &quoted,
            )));
        }
        let without_rowid = cell_i64(row.get(1)).unwrap_or(0) != 0;

        // Columns, including generated ones (hidden 2 = virtual, 3 = stored);
        // hidden 1 (virtual-table hidden columns) cannot appear here because
        // virtual tables were rejected above.
        let set = self
            .run_query(
                "SELECT name, type, \"notnull\", dflt_value, pk, hidden \
                 FROM main.pragma_table_xinfo(?1, ?2) WHERE hidden <> 1",
                &name_params,
            )
            .await?;
        let mut declared: Vec<(String, String)> = Vec::new();
        let mut key_ordered: Vec<(i64, String)> = Vec::new();
        let mut columns: Vec<RowEditColumn> = Vec::new();
        for row in &set.rows {
            let Some(name) = cell_str(row.first()) else {
                continue;
            };
            let decl = cell_str(row.get(1)).unwrap_or_default();
            let parsed = parse_decl(&decl);
            let notnull = cell_i64(row.get(2)).unwrap_or(0) != 0;
            let pk = cell_i64(row.get(4)).unwrap_or(0);
            let hidden = cell_i64(row.get(5)).unwrap_or(0);
            if pk > 0 {
                key_ordered.push((pk, name.clone()));
            }
            declared.push((name.clone(), decl));
            columns.push(RowEditColumn {
                name,
                data_type: parsed.base,
                char_max_length: parsed.char_len,
                precision: parsed.precision,
                scale: parsed.scale,
                nullable: !notnull,
                is_primary_key: pk > 0,
                is_identity: false, // rowid alias resolved below
                is_computed: hidden == 2 || hidden == 3,
                is_rowversion: false,
                default_definition: cell_str(row.get(3)),
                result_ordinal: None,
                enum_values: Vec::new(),
            });
        }
        if columns.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        }
        key_ordered.sort_by_key(|(pk, _)| *pk);
        let key_columns: Vec<String> = key_ordered.into_iter().map(|(_, name)| name).collect();

        if let Some(alias) = is_rowid_alias(&key_columns, &declared, without_rowid)
            && let Some(column) = columns.iter_mut().find(|c| c.name == alias)
        {
            column.is_identity = true;
        }

        // Outgoing foreign keys. SQLite constraints are unnamed; synthesize
        // stable names from the pragma's constraint id. A NULL "to" column
        // means the referenced table's primary key, in sequence order.
        let set = self
            .run_query(
                "SELECT id, seq, \"table\", \"from\", \"to\" \
                 FROM main.pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
                &name_params,
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeySpec> = Vec::new();
        for row in &set.rows {
            let Some(id) = cell_i64(row.first()) else {
                continue;
            };
            let seq = cell_i64(row.get(1)).unwrap_or(0);
            let ref_table = cell_str(row.get(2)).unwrap_or_default();
            let from = cell_str(row.get(3)).unwrap_or_default();
            let referenced_target = TableTarget {
                schema: target.schema.clone(),
                table: ref_table,
            };
            let to = match cell_str(row.get(4)) {
                Some(to) => to,
                None => {
                    // Implicit reference to the parent's primary key.
                    let parent_key = self.table_key_columns(&referenced_target).await?;
                    parent_key.get(seq as usize).cloned().unwrap_or_default()
                }
            };
            let constraint_name = format!("fk_{id}");
            match foreign_keys
                .iter_mut()
                .find(|spec| spec.constraint_name == constraint_name)
            {
                Some(spec) => spec.column_pairs.push((from, to)),
                None => {
                    let display_column = self.display_column_for(&referenced_target).await;
                    foreign_keys.push(ForeignKeySpec {
                        constraint_name,
                        referenced: referenced_target,
                        column_pairs: vec![(from, to)],
                        display_column,
                    });
                }
            }
        }

        // Incoming foreign keys: sweep the schema's tables for references to
        // this one. Bounded by the table count; row-editor opens are
        // user-paced and SQLite is local.
        let tables = self
            .run_query(
                "SELECT name FROM main.pragma_table_list WHERE schema = ?2 AND type = 'table'",
                &name_params,
            )
            .await?;
        let mut incoming_foreign_keys: Vec<IncomingForeignKey> = Vec::new();
        for row in &tables.rows {
            let Some(child) = cell_str(row.first()) else {
                continue;
            };
            if child.eq_ignore_ascii_case(&target.table) || child.starts_with("sqlite_") {
                continue;
            }
            let fk_rows = self
                .run_query(
                    "SELECT id, seq, \"table\", \"from\", \"to\" \
                     FROM main.pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
                    &[
                        SqlParam::String(child.clone()),
                        SqlParam::String(target.schema.clone()),
                    ],
                )
                .await?;
            for fk_row in &fk_rows.rows {
                let referenced = cell_str(fk_row.get(2)).unwrap_or_default();
                if !referenced.eq_ignore_ascii_case(&target.table) {
                    continue;
                }
                let Some(id) = cell_i64(fk_row.first()) else {
                    continue;
                };
                let seq = cell_i64(fk_row.get(1)).unwrap_or(0);
                let from = cell_str(fk_row.get(3)).unwrap_or_default();
                let to = match cell_str(fk_row.get(4)) {
                    Some(to) => to,
                    None => key_columns.get(seq as usize).cloned().unwrap_or_default(),
                };
                let constraint_name = format!("{child}_fk_{id}");
                match incoming_foreign_keys
                    .iter_mut()
                    .find(|spec| spec.constraint_name == constraint_name)
                {
                    Some(spec) => spec.column_pairs.push((from, to)),
                    None => incoming_foreign_keys.push(IncomingForeignKey {
                        constraint_name,
                        child: TableTarget {
                            schema: target.schema.clone(),
                            table: child.clone(),
                        },
                        column_pairs: vec![(from, to)],
                    }),
                }
            }
        }

        Ok(Ok(LoadedTable {
            columns,
            key_columns,
            foreign_keys,
            // CHECK constraints live only in the raw CREATE TABLE text;
            // SQLite has no catalog for them, so the server stays the sole
            // authority (constraint violations still surface on save).
            check_constraints: Vec::new(),
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
        sqlly_engines::sqlite::execute_params_on_client(self.client, sql, params).await
    }

    async fn begin(&mut self) -> StudioResult<()> {
        sqlly_engines::sqlite::execute_params_on_client(self.client, "BEGIN IMMEDIATE", &[])
            .await
            .map(|_| ())
    }

    async fn commit(&mut self) -> StudioResult<()> {
        sqlly_engines::sqlite::execute_params_on_client(self.client, "COMMIT", &[])
            .await
            .map(|_| ())
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        sqlly_engines::sqlite::execute_params_on_client(self.client, "ROLLBACK", &[])
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_editing::{
        DefaultRowEditingService, DeleteRowOutcome, DeleteRowRequest, FkSearchRequest,
        InsertRowOutcome, InsertRowRequest, RelatedRowsRequest, RowEditOpenOutcome,
        RowEditOpenRequest, RowEditSession, RowEditingService, UpdateRowOutcome, UpdateRowRequest,
    };
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};

    fn sqlite_profile(path: &str) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "sqlite-rowedit",
            path,
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Sqlite;
        profile
    }

    async fn seed(profile: &ConnectionProfile, sql: &str) {
        let client = sqlly_engines::sqlite::open_client(profile).await.unwrap();
        for statement in sql.split(';').filter(|s| !s.trim().is_empty()) {
            sqlly_engines::sqlite::execute_params_on_client(&client, statement, &[])
                .await
                .unwrap();
        }
    }

    /// Builds the open request exactly as the grid would: the query's real
    /// projected columns plus one row's values.
    async fn grid_request(
        profile: &ConnectionProfile,
        sql: &str,
        row: usize,
    ) -> RowEditOpenRequest {
        let client = sqlly_engines::sqlite::open_client(profile).await.unwrap();
        let sets = sqlly_engines::sqlite::query_rows_on_client(&client, sql, &[])
            .await
            .unwrap();
        let set = sets.into_iter().next().unwrap();
        RowEditOpenRequest {
            sql: sql.to_string(),
            result_set_index: 0,
            columns: set.columns,
            row: set.rows[row].clone(),
        }
    }

    fn editable(outcome: RowEditOpenOutcome) -> RowEditSession {
        match outcome {
            RowEditOpenOutcome::Editable(session) => *session,
            RowEditOpenOutcome::NotEditable { reason, .. } => {
                panic!("expected editable session, got: {reason}")
            }
        }
    }

    fn not_editable_reason(outcome: RowEditOpenOutcome) -> String {
        match outcome {
            RowEditOpenOutcome::NotEditable { reason, .. } => reason,
            RowEditOpenOutcome::Editable(_) => panic!("expected a not-editable outcome"),
        }
    }

    const PERSON_SCHEMA: &str = "CREATE TABLE person (\
         id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INT, note TEXT);\
         INSERT INTO person (name, age) VALUES ('Ada', 36), ('Grace', 45);";

    #[tokio::test]
    async fn sqlite_end_to_end_edit_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit.db");
        let profile = sqlite_profile(path.to_str().unwrap());
        seed(&profile, PERSON_SCHEMA).await;
        let service = DefaultRowEditingService::default();

        // Open: provenance resolves the base table, key, and identity.
        let request = grid_request(&profile, "SELECT id, name, age, note FROM person", 0).await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.target.schema, "main");
        assert_eq!(session.target.table, "person");
        assert_eq!(session.key_columns, vec!["id"]);
        assert!(session.rowversion_column.is_none());
        let id_col = &session.columns[0];
        assert!(id_col.is_primary_key && id_col.is_identity, "rowid alias");
        assert!(!id_col.is_editable());
        assert_eq!(session.original_row[1], CellValue::String("Ada".into()));

        // Update: guarded, parameterized, returns the refetched row.
        let update = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Bob".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&profile, update).await.unwrap() {
            UpdateRowOutcome::Updated { columns, row } => {
                let at = columns.iter().position(|c| c == "name").unwrap();
                assert_eq!(row[at], CellValue::String("Bob".into()));
            }
            other => panic!("expected Updated, got {other:?}"),
        }

        // Conflict (row changed): stale original values match nothing; the
        // current row comes back so the caller can show what changed.
        let stale = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Carol".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&profile, stale).await.unwrap() {
            UpdateRowOutcome::Conflict {
                current_columns,
                current_row,
            } => {
                let at = current_columns.iter().position(|c| c == "name").unwrap();
                assert_eq!(
                    current_row.expect("row still exists")[at],
                    CellValue::String("Bob".into())
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Conflict (row gone): out-of-band delete, then a guarded update.
        {
            let client = sqlly_engines::sqlite::open_client(&profile).await.unwrap();
            sqlly_engines::sqlite::execute_params_on_client(
                &client,
                "DELETE FROM person WHERE id = 2",
                &[],
            )
            .await
            .unwrap();
        }
        let gone = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(2))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("X".into()))],
            original_values: vec![("name".into(), CellValue::String("Grace".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&profile, gone).await.unwrap() {
            UpdateRowOutcome::Conflict { current_row, .. } => assert!(current_row.is_none()),
            other => panic!("expected Conflict for a deleted row, got {other:?}"),
        }

        // Insert: RETURNING resolves the engine-assigned rowid alias.
        let insert = InsertRowRequest {
            target: session.target.clone(),
            values: vec![
                ("name".into(), CellValue::String("Zed".into())),
                ("age".into(), CellValue::I64(30)),
            ],
            expressions: Vec::new(),
        };
        let inserted_id = match service.insert_row(&profile, insert).await.unwrap() {
            InsertRowOutcome::Inserted { columns, row } => {
                let id_at = columns.iter().position(|c| c == "id").unwrap();
                let note_at = columns.iter().position(|c| c == "note").unwrap();
                assert_eq!(row[note_at], CellValue::Null);
                match row[id_at] {
                    CellValue::I64(id) => {
                        assert!(id > 0, "identity resolved");
                        id
                    }
                    ref other => panic!("expected integer id, got {other:?}"),
                }
            }
        };

        // Delete: guarded by key + original values; then the same delete
        // reports the row as gone rather than silently succeeding.
        let delete = DeleteRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(inserted_id))],
            rowversion: None,
            original_values: vec![
                ("name".into(), CellValue::String("Zed".into())),
                ("age".into(), CellValue::I64(30)),
                ("note".into(), CellValue::Null),
            ],
        };
        assert!(matches!(
            service.delete_row(&profile, delete.clone()).await.unwrap(),
            DeleteRowOutcome::Deleted
        ));
        match service.delete_row(&profile, delete).await.unwrap() {
            DeleteRowOutcome::Conflict { current_row, .. } => assert!(current_row.is_none()),
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert!(
            service
                .fetch_row(
                    &profile,
                    &session.target,
                    &[("id".into(), CellValue::I64(inserted_id))]
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn sqlite_open_session_rejects_joins_missing_keys_and_expressions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gates.db");
        let profile = sqlite_profile(path.to_str().unwrap());
        seed(
            &profile,
            "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT);\
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INT);\
             INSERT INTO a (name) VALUES ('x');\
             INSERT INTO b (a_id) VALUES (1);\
             CREATE TABLE nopk (v INT); INSERT INTO nopk VALUES (9);",
        )
        .await;
        let service = DefaultRowEditingService::default();

        let join = grid_request(
            &profile,
            "SELECT a.id, b.id AS bid FROM a JOIN b ON b.a_id = a.id",
            0,
        )
        .await;
        let reason = not_editable_reason(service.open_session(&profile, join).await.unwrap());
        assert!(reason.contains("more than one table"), "{reason}");

        let missing_key = grid_request(&profile, "SELECT name FROM a", 0).await;
        let reason =
            not_editable_reason(service.open_session(&profile, missing_key).await.unwrap());
        assert!(reason.contains("key column"), "{reason}");

        let expression = grid_request(&profile, "SELECT 1 + 1 AS calc", 0).await;
        let reason = not_editable_reason(service.open_session(&profile, expression).await.unwrap());
        assert!(reason.contains("read-only"), "{reason}");

        let no_pk = grid_request(&profile, "SELECT v FROM nopk", 0).await;
        let reason = not_editable_reason(service.open_session(&profile, no_pk).await.unwrap());
        assert!(reason.contains("no primary key"), "{reason}");
    }

    #[tokio::test]
    async fn sqlite_open_insert_session_aligns_duplicate_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insert.db");
        let profile = sqlite_profile(path.to_str().unwrap());
        seed(&profile, PERSON_SCHEMA).await;
        let service = DefaultRowEditingService::default();

        // A projection without the key still opens an insert template, with
        // the row aligned into catalog order (NULL where not projected).
        let request = grid_request(&profile, "SELECT name FROM person", 1).await;
        let session = editable(
            service
                .open_insert_session(&profile, request)
                .await
                .unwrap(),
        );
        assert_eq!(session.target.table, "person");
        assert_eq!(session.original_row[0], CellValue::Null, "id not projected");
        assert_eq!(session.original_row[1], CellValue::String("Grace".into()));
        assert_eq!(session.original_row[2], CellValue::Null);
    }

    #[tokio::test]
    async fn sqlite_foreign_keys_search_related_and_display() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fk.db");
        let profile = sqlite_profile(path.to_str().unwrap());
        seed(
            &profile,
            "CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT);\
             CREATE TABLE orders (id INTEGER PRIMARY KEY, \
             customer_id INTEGER REFERENCES customer, amount REAL);\
             INSERT INTO customer (name) VALUES ('Acme'), ('Beta');\
             INSERT INTO orders (customer_id, amount) VALUES (1, 9.5), (1, 3.25), (2, 7.0);",
        )
        .await;
        let service = DefaultRowEditingService::default();

        // Orders session: outgoing FK resolved through the implicit-PK
        // reference, display column found, current label preloaded.
        let request = grid_request(&profile, "SELECT id, customer_id, amount FROM orders", 0).await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.foreign_keys.len(), 1);
        let fk = &session.foreign_keys[0];
        assert_eq!(fk.referenced.table, "customer");
        assert_eq!(
            fk.column_pairs,
            vec![("customer_id".to_string(), "id".to_string())]
        );
        assert_eq!(fk.display_column.as_deref(), Some("name"));
        assert_eq!(session.fk_display[0].as_deref(), Some("Acme"));

        // FK autocomplete: filtered, bounded, labeled.
        let search = service
            .fk_search(
                &profile,
                FkSearchRequest {
                    referenced: fk.referenced.clone(),
                    key_columns: vec!["id".into()],
                    display_column: Some("name".into()),
                    query: "ac".into(),
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(search.candidates.len(), 1);
        assert_eq!(search.candidates[0].label, "Acme");
        assert!(!search.truncated);

        // Related rows: bounded fetch plus a runnable open query.
        let related = service
            .related_rows(
                &profile,
                RelatedRowsRequest {
                    table: session.target.clone(),
                    predicates: vec![("customer_id".into(), CellValue::I64(1))],
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(related.rows.len(), 1);
        assert!(related.truncated, "two rows exist, limit is one");
        assert!(
            related.open_sql.contains("\"main\".\"orders\""),
            "{}",
            related.open_sql
        );

        // Customer session sees the incoming FK from orders.
        let request = grid_request(&profile, "SELECT id, name FROM customer", 0).await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.incoming_foreign_keys.len(), 1);
        assert_eq!(session.incoming_foreign_keys[0].child.table, "orders");
        assert_eq!(
            session.incoming_foreign_keys[0].column_pairs,
            vec![("customer_id".to_string(), "id".to_string())]
        );
    }

    #[tokio::test]
    async fn sqlite_policy_gates_block_editing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.db");
        let mut profile = sqlite_profile(path.to_str().unwrap());
        seed(&profile, PERSON_SCHEMA).await;
        profile.block_all_but_select = true;
        let service = DefaultRowEditingService::default();

        let request = grid_request(&profile, "SELECT id, name, age, note FROM person", 0).await;
        let reason = not_editable_reason(service.open_session(&profile, request).await.unwrap());
        assert!(reason.contains("SELECT statements only"), "{reason}");

        let update = UpdateRowRequest {
            target: crate::row_editing::TableTarget {
                schema: "main".into(),
                table: "person".into(),
            },
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("X".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        assert!(matches!(
            service.update_row(&profile, update).await,
            Err(sqlly_core::StudioError::PermissionDeniedMessage(_))
        ));
    }

    #[test]
    fn parse_decl_maps_families() {
        assert_eq!(
            parse_decl("VARCHAR(30)"),
            SqliteDecl {
                base: "varchar".into(),
                char_len: Some(30),
                precision: None,
                scale: None,
            }
        );
        assert_eq!(
            parse_decl("DECIMAL(10,2)"),
            SqliteDecl {
                base: "decimal".into(),
                char_len: None,
                precision: Some(10),
                scale: Some(2),
            }
        );
        assert_eq!(parse_decl(" integer ").base, "integer");
        assert_eq!(parse_decl("").base, "");
        assert_eq!(parse_decl("INT").base, "int");
    }

    #[test]
    fn rowid_alias_requires_exact_integer_single_key_rowid_table() {
        let declared = vec![
            ("id".to_string(), "INTEGER".to_string()),
            ("name".to_string(), "TEXT".to_string()),
        ];
        assert_eq!(
            is_rowid_alias(&["id".to_string()], &declared, false),
            Some("id".to_string())
        );
        // WITHOUT ROWID tables have no rowid to alias.
        assert_eq!(is_rowid_alias(&["id".to_string()], &declared, true), None);
        // `INT` is not the alias spelling.
        let int_decl = vec![("id".to_string(), "INT".to_string())];
        assert_eq!(is_rowid_alias(&["id".to_string()], &int_decl, false), None);
        // Composite keys never alias the rowid.
        assert_eq!(
            is_rowid_alias(&["id".to_string(), "b".to_string()], &declared, false),
            None
        );
    }
}
