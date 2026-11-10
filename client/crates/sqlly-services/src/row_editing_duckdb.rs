//! DuckDB row-editing engine.
//!
//! DuckDB's C API describes a prepared statement's result columns (names and
//! types) but, unlike SQLite and MySQL, does not report which base table and
//! column each projected value came from. Provenance is therefore derived in
//! two steps that both consult the engine: `DESCRIBE <statement>` yields the
//! statement's real projected column names (and rejects anything DuckDB
//! cannot plan), and the statement's own FROM clause is read to find the one
//! base table it selects from; every projected label is then resolved back
//! to that table's column through `duckdb_columns()`, so only a column the
//! catalog knows becomes editable, with the catalog's own spelling. Aliased
//! columns trace through their expression; computed expressions, joins,
//! set operations, and multi-statement scripts are read-only, exactly as on
//! the other engines.
//!
//! Table metadata comes from `duckdb_columns()` and `duckdb_constraints()`,
//! both parameterized. A sequence-backed default (`nextval('…')`) is
//! DuckDB's auto-increment and is surfaced as an identity column. Writes run
//! inside `BEGIN TRANSACTION … COMMIT` and inserts use `RETURNING *`.

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioResult};
use sqlly_intel::query::column_provenance::StatementProvenance;
use sqlly_lang::sqllex::split_statements;
use sqlly_lang::sqlmodel::Dialect;

use crate::row_editing::{
    CheckConstraintSpec, DescribeColumn, ForeignKeySpec, IncomingForeignKey, LoadedTable,
    RowEditColumn, TableTarget, fill_enum_values_from_checks,
};
use crate::row_editing_engines::RowEditEngine;
use crate::row_editing_sql as dialect;

/// `COALESCE(constraint_name, …)`: older DuckDB releases had no
/// `constraint_name` column, so the type and index synthesize one.
const CONSTRAINT_NAME: &str =
    "COALESCE(constraint_name, constraint_type || '_' || CAST(constraint_index AS VARCHAR))";

/// A declared DuckDB type split into its base name and any size arguments:
/// `DECIMAL(10,2)` → base `decimal` with precision/scale, `VARCHAR(30)` →
/// base `varchar` with a character length (DuckDB does not enforce the
/// length, but the editor still validates against it, like SQLite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuckDecl {
    pub base: String,
    pub char_len: Option<i64>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
}

pub(crate) fn parse_decl(decl: &str) -> DuckDecl {
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
    let is_char = matches!(
        base.as_str(),
        "varchar" | "char" | "text" | "string" | "bpchar"
    );
    let is_decimal = matches!(base.as_str(), "decimal" | "numeric");
    DuckDecl {
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

/// The one base table a statement selects from, as `(schema, table)`; the
/// schema is `default_schema` when the statement leaves it unqualified.
/// `None` when the statement has no FROM, joins several tables, or reads a
/// derived table / CTE (traced one level down when it wraps one base table).
pub(crate) fn single_source_table(
    provenance: &StatementProvenance,
    labels: &[String],
    default_schema: &str,
) -> Option<TableTarget> {
    let mut target: Option<TableTarget> = None;
    for (index, label) in labels.iter().enumerate() {
        let occurrences = labels
            .iter()
            .filter(|l| l.eq_ignore_ascii_case(label))
            .count();
        let Some(origin) = provenance.resolve_origin(label, index, occurrences) else {
            continue;
        };
        let this = target_from_chain(&origin.table, default_schema)?;
        match &target {
            None => target = Some(this),
            Some(existing) if same_target(existing, &this) => {}
            Some(_) => return None,
        }
    }
    target
}

/// `[table]`, `[schema, table]`, or `[catalog, schema, table]` into a
/// target; catalogs beyond three parts are not a table reference.
fn target_from_chain(chain: &[String], default_schema: &str) -> Option<TableTarget> {
    match chain {
        [table] => Some(TableTarget {
            schema: default_schema.to_string(),
            table: table.clone(),
        }),
        [schema, table] | [_, schema, table] => Some(TableTarget {
            schema: schema.clone(),
            table: table.clone(),
        }),
        _ => None,
    }
}

/// DuckDB identifiers are case-insensitive unless quoted; two references
/// that differ only in case name the same table.
fn same_target(left: &TableTarget, right: &TableTarget) -> bool {
    left.schema.eq_ignore_ascii_case(&right.schema) && left.table.eq_ignore_ascii_case(&right.table)
}

/// Exactly one statement in the text, or `Err(reason)`. DuckDB executes
/// every statement of a multi-statement string when asked to describe it,
/// so the guard runs before anything reaches the engine.
fn single_statement(sql: &str) -> Result<String, sqlly_core::DisplayMessage> {
    let statements: Vec<&str> = split_statements(sql, Dialect::Postgres)
        .into_iter()
        .map(|slice| slice.text.trim())
        .filter(|text| !text.is_empty() && !is_only_comments(text))
        .collect();
    match statements.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(crate::row_editing::not_editable::empty_statement()),
        _ => Err(crate::row_editing::not_editable::several_statements()),
    }
}

fn is_only_comments(text: &str) -> bool {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix("--") {
            rest = after.find('\n').map(|i| &after[i + 1..]).unwrap_or("");
        } else if let Some(after) = trimmed.strip_prefix("/*") {
            rest = after.find("*/").map(|i| &after[i + 2..]).unwrap_or("");
        } else {
            return trimmed.is_empty();
        }
    }
}

pub(crate) struct DuckDbEngine<'a> {
    client: &'a sqlly_engines::duckdb::DuckDbClient,
    db: DatabaseType,
}

impl<'a> DuckDbEngine<'a> {
    pub(crate) fn new(client: &'a sqlly_engines::duckdb::DuckDbClient, db: DatabaseType) -> Self {
        Self { client, db }
    }

    async fn run_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let sets = sqlly_engines::duckdb::query_rows_on_client(self.client, sql, params).await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    async fn run_batch(&self, sql: &str) -> StudioResult<()> {
        sqlly_engines::duckdb::execute_batch_on_client(self.client, sql).await
    }

    /// The catalog's own spelling of a table's columns, in column order.
    /// Empty when the table does not exist (or is a view).
    async fn catalog_columns(&self, target: &TableTarget) -> StudioResult<Vec<String>> {
        let set = self
            .run_query(
                "SELECT column_name FROM duckdb_columns() \
                 WHERE database_name = current_database() \
                 AND lower(schema_name) = lower(?) AND lower(table_name) = lower(?) \
                 ORDER BY column_index",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect())
    }

    /// The catalog's spelling of a table (schema and name) when it exists.
    async fn catalog_table(&self, target: &TableTarget) -> StudioResult<Option<TableTarget>> {
        let set = self
            .run_query(
                "SELECT schema_name, table_name FROM duckdb_tables() \
                 WHERE database_name = current_database() \
                 AND lower(schema_name) = lower(?) AND lower(table_name) = lower(?)",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        Ok(set.rows.first().and_then(|row| {
            Some(TableTarget {
                schema: cell_str(row.first())?,
                table: cell_str(row.get(1))?,
            })
        }))
    }

    /// Primary-key column names of a table, in key order.
    async fn table_key_columns(&self, target: &TableTarget) -> StudioResult<Vec<String>> {
        let set = self
            .run_query(
                "SELECT unnest(constraint_column_names) FROM duckdb_constraints() \
                 WHERE database_name = current_database() AND schema_name = ? AND table_name = ? \
                 AND constraint_type = 'PRIMARY KEY'",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        Ok(set
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect())
    }

    /// Best-effort display column for a referenced table: a text column
    /// with a name-like identifier.
    async fn display_column_for(&self, target: &TableTarget) -> Option<String> {
        let set = self
            .run_query(
                "SELECT column_name, data_type FROM duckdb_columns() \
                 WHERE database_name = current_database() AND schema_name = ? AND table_name = ? \
                 ORDER BY column_index",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
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
            if !matches!(
                base.as_str(),
                "varchar" | "char" | "text" | "string" | "bpchar"
            ) {
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

fn cell_bool(cell: Option<&CellValue>) -> bool {
    matches!(cell, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
}

/// The driver renders DuckDB LIST values as JSON text (`["a","b"]`), which
/// is how `constraint_column_names` arrives here.
fn json_list(cell: Option<&CellValue>) -> Vec<String> {
    match cell {
        Some(CellValue::String(text)) => {
            serde_json::from_str::<Vec<String>>(text).unwrap_or_else(|_| vec![text.clone()])
        }
        _ => Vec::new(),
    }
}

#[async_trait]
impl RowEditEngine for DuckDbEngine<'_> {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, sqlly_core::DisplayMessage>> {
        let statement = match single_statement(sql) {
            Ok(statement) => statement,
            Err(reason) => return Ok(Err(reason)),
        };
        // 1. The engine's view of the projection: DESCRIBE plans the
        //    statement without running it and lists its output columns.
        let described = match self.run_query(&format!("DESCRIBE {statement}"), &[]).await {
            Ok(set) => set,
            Err(err) => {
                return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                    err,
                )));
            }
        };
        let labels: Vec<String> = described
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect();
        if labels.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::result_shape_unknown()));
        }

        // 2. The one base table the statement reads, from its FROM clause.
        let unsourced = |labels: &[String]| -> Vec<DescribeColumn> {
            labels
                .iter()
                .map(|name| DescribeColumn {
                    name: name.clone(),
                    ..DescribeColumn::default()
                })
                .collect()
        };
        let Some(provenance) = StatementProvenance::parse(&statement) else {
            return Ok(Ok(unsourced(&labels)));
        };
        let default_schema = self.db.default_schema().unwrap_or("main");
        let Some(source) = single_source_table(&provenance, &labels, default_schema) else {
            return Ok(Ok(unsourced(&labels)));
        };
        // 3. Validate against the catalog: the table must be a base table
        //    and each traced column one of its columns, spelled as stored.
        let Some(table) = self.catalog_table(&source).await? else {
            return Ok(Ok(unsourced(&labels)));
        };
        let catalog = self.catalog_columns(&table).await?;
        let columns = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let occurrences = labels
                    .iter()
                    .filter(|l| l.eq_ignore_ascii_case(label))
                    .count();
                let source_column = provenance
                    .resolve_origin(label, index, occurrences)
                    .and_then(|origin| {
                        catalog
                            .iter()
                            .find(|c| c.eq_ignore_ascii_case(&origin.column))
                            .cloned()
                    });
                match source_column {
                    Some(column) => DescribeColumn {
                        name: label.clone(),
                        is_hidden: false,
                        source_schema: Some(table.schema.clone()),
                        source_table: Some(table.table.clone()),
                        source_column: Some(column),
                    },
                    None => DescribeColumn {
                        name: label.clone(),
                        ..DescribeColumn::default()
                    },
                }
            })
            .collect();
        Ok(Ok(columns))
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

        // Base table or not: views live in duckdb_views(), tables here.
        let set = self
            .run_query(
                "SELECT (SELECT COUNT(*) FROM duckdb_tables() WHERE database_name = current_database() AND schema_name = ? AND table_name = ?) AS tables, \
                 (SELECT COUNT(*) FROM duckdb_views() WHERE database_name = current_database() AND schema_name = ? AND view_name = ?) AS views",
                &[
                    name_params[0].clone(),
                    name_params[1].clone(),
                    name_params[0].clone(),
                    name_params[1].clone(),
                ],
            )
            .await?;
        let (tables, views) = set
            .rows
            .first()
            .map(|row| {
                (
                    cell_i64(row.first()).unwrap_or(0),
                    cell_i64(row.get(1)).unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        if tables == 0 {
            return Ok(Err(if views > 0 {
                crate::row_editing::not_editable::not_base_table(&quoted)
            } else {
                crate::row_editing::not_editable::table_not_found(&quoted)
            }));
        }

        let set = self
            .run_query(
                "SELECT column_name, data_type, is_nullable, column_default, \
                 character_maximum_length, numeric_precision, numeric_scale \
                 FROM duckdb_columns() \
                 WHERE database_name = current_database() AND schema_name = ? AND table_name = ? \
                 ORDER BY column_index",
                &name_params,
            )
            .await?;
        let mut columns: Vec<RowEditColumn> = Vec::new();
        for row in &set.rows {
            let Some(name) = cell_str(row.first()) else {
                continue;
            };
            let decl = cell_str(row.get(1)).unwrap_or_default();
            let parsed = parse_decl(&decl);
            let default_definition = cell_str(row.get(3));
            let is_identity = default_definition
                .as_deref()
                .is_some_and(|d| d.trim_start().to_lowercase().starts_with("nextval("));
            columns.push(RowEditColumn {
                name,
                data_type: parsed.base,
                char_max_length: cell_i64(row.get(4)).or(parsed.char_len),
                precision: cell_i64(row.get(5))
                    .and_then(|p| u8::try_from(p).ok())
                    .or(parsed.precision),
                scale: cell_i64(row.get(6))
                    .and_then(|s| u8::try_from(s).ok())
                    .or(parsed.scale),
                nullable: cell_bool(row.get(2)),
                is_primary_key: false, // filled in below
                is_identity,
                is_computed: false,
                is_rowversion: false,
                default_definition,
                result_ordinal: None,
                enum_values: Vec::new(),
            });
        }
        if columns.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        }

        let key_columns = self.table_key_columns(target).await?;
        for column in &mut columns {
            column.is_primary_key = key_columns.contains(&column.name);
        }

        // Constraints: outgoing foreign keys, CHECKs. DuckDB foreign keys are
        // same-schema, so the referenced table takes this table's schema.
        let set = self
            .run_query(
                &format!(
                    "SELECT {CONSTRAINT_NAME} AS name, constraint_type, referenced_table, \
                     constraint_column_names AS from_columns, \
                     referenced_column_names AS to_columns, \
                     COALESCE(expression, constraint_text) AS expression \
                     FROM duckdb_constraints() \
                     WHERE database_name = current_database() AND schema_name = ? AND table_name = ? \
                     AND constraint_type IN ('FOREIGN KEY', 'CHECK') ORDER BY constraint_index"
                ),
                &name_params,
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeySpec> = Vec::new();
        let mut check_constraints: Vec<CheckConstraintSpec> = Vec::new();
        for row in &set.rows {
            let Some(name) = cell_str(row.first()) else {
                continue;
            };
            let kind = cell_str(row.get(1)).unwrap_or_default();
            if kind == "CHECK" {
                if let Some(definition) = cell_str(row.get(5)) {
                    check_constraints.push(CheckConstraintSpec { name, definition });
                }
                continue;
            }
            let referenced_target = TableTarget {
                schema: target.schema.clone(),
                table: cell_str(row.get(2)).unwrap_or_default(),
            };
            let from = json_list(row.get(3));
            let mut to = json_list(row.get(4));
            if to.is_empty() {
                to = self.table_key_columns(&referenced_target).await?;
            }
            let display_column = self.display_column_for(&referenced_target).await;
            foreign_keys.push(ForeignKeySpec {
                constraint_name: name,
                referenced: referenced_target,
                column_pairs: from.into_iter().zip(to).collect(),
                display_column,
            });
        }
        fill_enum_values_from_checks(&mut columns, &check_constraints);

        // Incoming foreign keys: every table in the schema that references
        // this one.
        let set = self
            .run_query(
                &format!(
                    "SELECT {CONSTRAINT_NAME} AS name, table_name, \
                     constraint_column_names AS from_columns, \
                     referenced_column_names AS to_columns \
                     FROM duckdb_constraints() \
                     WHERE database_name = current_database() AND schema_name = ? \
                     AND constraint_type = 'FOREIGN KEY' AND lower(referenced_table) = lower(?) \
                     ORDER BY table_name, constraint_index"
                ),
                &name_params,
            )
            .await?;
        let mut incoming_foreign_keys: Vec<IncomingForeignKey> = Vec::new();
        for row in &set.rows {
            let Some(name) = cell_str(row.first()) else {
                continue;
            };
            let child = cell_str(row.get(1)).unwrap_or_default();
            let from = json_list(row.get(2));
            let mut to = json_list(row.get(3));
            if to.is_empty() {
                to = key_columns.clone();
            }
            incoming_foreign_keys.push(IncomingForeignKey {
                constraint_name: format!("{child}.{name}"),
                child: TableTarget {
                    schema: target.schema.clone(),
                    table: child,
                },
                column_pairs: from.into_iter().zip(to).collect(),
            });
        }

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
        sqlly_engines::duckdb::execute_params_on_client(self.client, sql, params).await
    }

    async fn begin(&mut self) -> StudioResult<()> {
        self.run_batch("BEGIN TRANSACTION").await
    }

    async fn commit(&mut self) -> StudioResult<()> {
        self.run_batch("COMMIT").await
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        self.run_batch("ROLLBACK").await
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

    /// A private file database per test: `:memory:` is one shared scratch
    /// database for the whole process.
    fn duckdb_profile(dir: &tempfile::TempDir, name: &str) -> ConnectionProfile {
        let path = dir.path().join(name);
        let mut profile = ConnectionProfile::sql_login(
            "duckdb-rowedit",
            path.to_string_lossy().as_ref(),
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::DuckDb;
        profile
    }

    async fn seed(profile: &ConnectionProfile, sql: &str) {
        let client = sqlly_engines::duckdb::open_client(profile).await.unwrap();
        sqlly_engines::duckdb::execute_batch_on_client(&client, sql)
            .await
            .unwrap();
    }

    /// Builds the open request exactly as the grid would: the query's real
    /// projected columns plus one row's values.
    async fn grid_request(
        profile: &ConnectionProfile,
        sql: &str,
        row: usize,
    ) -> RowEditOpenRequest {
        let client = sqlly_engines::duckdb::open_client(profile).await.unwrap();
        let sets = sqlly_engines::duckdb::query_rows_on_client(&client, sql, &[])
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

    const PERSON_SCHEMA: &str = "CREATE SEQUENCE person_id;\
         CREATE TABLE person (\
         id INTEGER PRIMARY KEY DEFAULT nextval('person_id'), name VARCHAR NOT NULL, \
         age INTEGER, note VARCHAR, amount DECIMAL(10,2), active BOOLEAN DEFAULT true);\
         INSERT INTO person (name, age) VALUES ('Ada', 36), ('Grace', 45);";

    #[tokio::test]
    async fn duckdb_end_to_end_edit_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let profile = duckdb_profile(&dir, "edit.duckdb");
        seed(&profile, PERSON_SCHEMA).await;
        let service = DefaultRowEditingService::default();

        // Open: provenance resolves the base table, key, identity and types.
        let request = grid_request(
            &profile,
            "SELECT id, name, age, note, amount, active FROM person ORDER BY id",
            0,
        )
        .await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.target.schema, "main");
        assert_eq!(session.target.table, "person");
        assert_eq!(session.key_columns, vec!["id"]);
        assert!(session.rowversion_column.is_none());
        let id_col = &session.columns[0];
        assert!(
            id_col.is_primary_key && id_col.is_identity,
            "sequence default"
        );
        assert!(!id_col.is_editable());
        assert_eq!(session.columns[1].data_type, "varchar");
        assert!(!session.columns[1].nullable);
        assert_eq!(session.columns[4].data_type, "decimal");
        assert_eq!(session.columns[4].precision, Some(10));
        assert_eq!(session.columns[4].scale, Some(2));
        assert!(
            session.columns[5].default_definition.is_some(),
            "boolean default kept"
        );
        assert_eq!(session.original_row[1], CellValue::String("Ada".into()));
        assert_eq!(session.original_row[5], CellValue::Bool(true));

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
        seed(&profile, "DELETE FROM person WHERE id = 2").await;
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

        // Insert: RETURNING resolves the sequence-assigned id and defaults.
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
                let active_at = columns.iter().position(|c| c == "active").unwrap();
                assert_eq!(row[note_at], CellValue::Null);
                assert_eq!(row[active_at], CellValue::Bool(true));
                match row[id_at] {
                    CellValue::I64(id) => {
                        assert!(id > 2, "sequence advanced past the seeded rows");
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
    async fn duckdb_open_session_rejects_joins_missing_keys_expressions_and_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let profile = duckdb_profile(&dir, "gates.duckdb");
        seed(
            &profile,
            "CREATE TABLE a (id INTEGER PRIMARY KEY, name VARCHAR);\
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER);\
             INSERT INTO a VALUES (1, 'x');\
             INSERT INTO b VALUES (1, 1);\
             CREATE TABLE nopk (v INTEGER); INSERT INTO nopk VALUES (9);\
             CREATE VIEW va AS SELECT id, name FROM a;",
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
        assert!(reason.contains("read-only"), "{reason}");

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

        let view = grid_request(&profile, "SELECT id, name FROM va", 0).await;
        let reason = not_editable_reason(service.open_session(&profile, view).await.unwrap());
        assert!(reason.contains("read-only"), "{reason}");

        // Two statements would both run under DESCRIBE; the guard refuses
        // before anything reaches the engine.
        let script = RowEditOpenRequest {
            sql: "SELECT id, name FROM a; DELETE FROM a".to_string(),
            result_set_index: 0,
            columns: grid_request(&profile, "SELECT id, name FROM a", 0)
                .await
                .columns,
            row: vec![CellValue::I64(1), CellValue::String("x".into())],
        };
        let reason = not_editable_reason(service.open_session(&profile, script).await.unwrap());
        assert!(reason.contains("several statements"), "{reason}");
        let client = sqlly_engines::duckdb::open_client(&profile).await.unwrap();
        let rows =
            sqlly_engines::duckdb::query_rows_on_client(&client, "SELECT COUNT(*) FROM a", &[])
                .await
                .unwrap();
        assert_eq!(rows[0].rows[0][0], CellValue::I64(1), "DELETE never ran");
    }

    #[tokio::test]
    async fn duckdb_aliases_and_case_resolve_to_catalog_columns() {
        let dir = tempfile::tempdir().unwrap();
        let profile = duckdb_profile(&dir, "alias.duckdb");
        seed(&profile, PERSON_SCHEMA).await;
        let service = DefaultRowEditingService::default();

        // An alias, a differently-cased column, and a schema-qualified
        // table all trace back to the catalog's own column names.
        let request = grid_request(
            &profile,
            "SELECT p.ID, p.name AS who, AGE FROM main.person p WHERE p.id = 1",
            0,
        )
        .await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.target.table, "person");
        let by_name = |name: &str| {
            session
                .columns
                .iter()
                .find(|c| c.name == name)
                .expect(name)
                .result_ordinal
        };
        assert_eq!(by_name("id"), Some(0));
        assert_eq!(by_name("name"), Some(1));
        assert_eq!(by_name("age"), Some(2));
        assert_eq!(by_name("note"), None);

        // A CTE over the table still resolves one level down.
        let request = grid_request(
            &profile,
            "WITH p AS (SELECT id, name FROM person) SELECT id, name FROM p ORDER BY id",
            1,
        )
        .await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.original_row[1], CellValue::String("Grace".into()));
    }

    #[tokio::test]
    async fn duckdb_open_insert_session_aligns_duplicate_seed() {
        let dir = tempfile::tempdir().unwrap();
        let profile = duckdb_profile(&dir, "insert.duckdb");
        seed(&profile, PERSON_SCHEMA).await;
        let service = DefaultRowEditingService::default();

        let request = grid_request(&profile, "SELECT name FROM person ORDER BY id", 1).await;
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
    async fn duckdb_foreign_keys_search_related_display_and_checks() {
        let dir = tempfile::tempdir().unwrap();
        let profile = duckdb_profile(&dir, "fk.duckdb");
        seed(
            &profile,
            "CREATE TABLE customer (id INTEGER PRIMARY KEY, name VARCHAR);\
             CREATE TABLE orders (id INTEGER PRIMARY KEY, \
             customer_id INTEGER REFERENCES customer(id), amount DOUBLE, \
             status VARCHAR CHECK (status IN ('new', 'paid')));\
             INSERT INTO customer VALUES (1, 'Acme'), (2, 'Beta');\
             INSERT INTO orders VALUES (10, 1, 9.5, 'new'), (11, 1, 3.25, 'paid'), (12, 2, 7.0, 'new');",
        )
        .await;
        let service = DefaultRowEditingService::default();

        let request = grid_request(
            &profile,
            "SELECT id, customer_id, amount, status FROM orders ORDER BY id",
            0,
        )
        .await;
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
        assert_eq!(session.check_constraints.len(), 1);
        let status = session.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(
            status.enum_values,
            vec!["new".to_string(), "paid".to_string()]
        );

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

        let request = grid_request(&profile, "SELECT id, name FROM customer ORDER BY id", 0).await;
        let session = editable(service.open_session(&profile, request).await.unwrap());
        assert_eq!(session.incoming_foreign_keys.len(), 1);
        assert_eq!(session.incoming_foreign_keys[0].child.table, "orders");
        assert_eq!(
            session.incoming_foreign_keys[0].column_pairs,
            vec![("customer_id".to_string(), "id".to_string())]
        );
    }

    #[tokio::test]
    async fn duckdb_policy_gates_block_editing() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = duckdb_profile(&dir, "policy.duckdb");
        seed(&profile, PERSON_SCHEMA).await;
        profile.block_all_but_select = true;
        let service = DefaultRowEditingService::default();

        let request = grid_request(&profile, "SELECT id, name FROM person", 0).await;
        let reason = not_editable_reason(service.open_session(&profile, request).await.unwrap());
        assert!(reason.contains("SELECT statements only"), "{reason}");
    }

    #[test]
    fn parse_decl_maps_families() {
        assert_eq!(
            parse_decl("VARCHAR(30)"),
            DuckDecl {
                base: "varchar".into(),
                char_len: Some(30),
                precision: None,
                scale: None,
            }
        );
        assert_eq!(
            parse_decl("DECIMAL(10,2)"),
            DuckDecl {
                base: "decimal".into(),
                char_len: None,
                precision: Some(10),
                scale: Some(2),
            }
        );
        assert_eq!(
            parse_decl("TIMESTAMP WITH TIME ZONE").base,
            "timestamp with time zone"
        );
        assert_eq!(parse_decl("INTEGER[]").base, "integer[]");
        assert_eq!(parse_decl("STRUCT(a INTEGER)").base, "struct");
    }

    #[test]
    fn single_statement_guard_and_source_table_resolution() {
        assert!(single_statement("SELECT 1; SELECT 2").is_err());
        assert!(single_statement("-- only a comment").is_err());
        assert_eq!(
            single_statement("  SELECT 1;  ").unwrap(),
            "SELECT 1".to_string()
        );

        let labels = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let p = StatementProvenance::parse("SELECT id, name AS who FROM person").unwrap();
        assert_eq!(
            single_source_table(&p, &labels(&["id", "who"]), "main"),
            Some(TableTarget {
                schema: "main".into(),
                table: "person".into()
            })
        );
        let p = StatementProvenance::parse("SELECT p.id FROM app.person p").unwrap();
        assert_eq!(
            single_source_table(&p, &labels(&["id"]), "main")
                .unwrap()
                .schema,
            "app"
        );
        let p = StatementProvenance::parse("SELECT p.id FROM memory.app.person p").unwrap();
        assert_eq!(
            single_source_table(&p, &labels(&["id"]), "main")
                .unwrap()
                .table,
            "person"
        );
        let p =
            StatementProvenance::parse("SELECT a.id, b.id AS bid FROM a JOIN b ON b.a_id = a.id")
                .unwrap();
        assert_eq!(
            single_source_table(&p, &labels(&["id", "bid"]), "main"),
            None
        );
        let p = StatementProvenance::parse("SELECT 1 + 1 AS calc").unwrap();
        assert_eq!(single_source_table(&p, &labels(&["calc"]), "main"), None);
    }
}
