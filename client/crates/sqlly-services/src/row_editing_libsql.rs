//! libSQL / Turso row-editing engine.
//!
//! SQLite's dialect over the Hrana HTTP protocol. Unlike the in-process
//! SQLite driver, Hrana reports a result's column names and declared types
//! but not the base table and column each value came from, so provenance
//! is derived the way the DuckDB lane derives it: the statement is planned
//! server-side inside `SELECT * FROM (…) LIMIT 0` to learn its real result
//! labels (and to reject anything the server cannot prepare), the one base
//! table its FROM clause reads is found in the statement text, and every
//! label is traced back to that table's column through `pragma_table_xinfo`
//! — so only a column the catalog knows becomes editable, spelled as the
//! catalog spells it. Joins, expressions, set operations, views (read from
//! `pragma_table_list`) and multi-statement scripts are read-only.
//!
//! Keys come from `pragma_table_xinfo`'s `pk` ordinal; a single `INTEGER
//! PRIMARY KEY` on a rowid table is the rowid alias and is surfaced as an
//! identity column, exactly like the SQLite lane. A rowid table with no
//! declared primary key is still editable through its hidden `rowid`: the
//! lane exposes `rowid` as an identity key column, which the SELECT must
//! project (`SELECT rowid, * FROM t`) for a row to be identified. Foreign
//! keys come from `pragma_foreign_key_list`, CHECK constraints from the
//! table's own `CREATE TABLE` text in `sqlite_master`.
//!
//! Every write runs on one server-side stream held open through its Hrana
//! baton, so `BEGIN IMMEDIATE`, the guarded statement(s), the refetch and
//! the `COMMIT`/`ROLLBACK` are one transaction; a staged multi-row save
//! travels as a single conditional batch whose first failure stops the
//! rest, and the transaction commits only after every affected-row count
//! has been checked.

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{
    CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioError, StudioResult,
};
use sqlly_engines::libsql::hrana::{StatementOutcome, split_script};
use sqlly_engines::libsql::{LibSqlClient, LibSqlStream};
use sqlly_intel::query::column_provenance::StatementProvenance;

use crate::row_editing::staged::StagedStatement;
use crate::row_editing::{
    CheckConstraintSpec, DescribeColumn, ForeignKeySpec, IncomingForeignKey, InsertRowRequest,
    LoadedTable, RowEditColumn, TableTarget, fill_enum_values_from_checks,
};
use crate::row_editing_duckdb::single_source_table;
use crate::row_editing_engines::{
    BatchRun, RowEditEngine, check_batch_counts, expect_single_inserted_row,
};
use crate::row_editing_sql as dialect;
use crate::row_editing_sqlite::{is_rowid_alias, parse_decl};

/// The key column exposed for a rowid table without a declared primary key.
pub(crate) const ROWID_KEY: &str = "rowid";

/// SQLite's spellings of the hidden rowid.
pub(crate) fn is_rowid_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

/// Exactly one statement in the text, or `Err(reason)`. The server refuses
/// a request holding two statements anyway, but the guard gives the user a
/// reason instead of the server's protocol error.
pub(crate) fn single_statement(sql: &str) -> Result<String, sqlly_core::DisplayMessage> {
    let statements = split_script(sql);
    match statements.as_slice() {
        [one] => Ok(one.trim().trim_end_matches(';').trim().to_string()),
        [] => Err(crate::row_editing::not_editable::empty_statement()),
        _ => Err(crate::row_editing::not_editable::several_statements()),
    }
}

/// The server-side probe that plans the statement and answers with its
/// result columns without producing a row.
pub(crate) fn probe_sql(statement: &str) -> String {
    format!("SELECT * FROM ({statement}) LIMIT 0")
}

/// `SELECT * FROM t …` → `SELECT rowid AS rowid, * FROM t …`, so a refetch
/// of a rowid-keyed table carries the key the editor addresses rows by.
pub(crate) fn with_rowid_projection(sql: &str) -> String {
    match sql.strip_prefix("SELECT * FROM ") {
        Some(rest) => format!("SELECT rowid AS rowid, * FROM {rest}"),
        None => sql.to_string(),
    }
}

/// `… RETURNING *` → `… RETURNING rowid AS rowid, *`.
pub(crate) fn with_rowid_returning(sql: &str) -> String {
    match sql.strip_suffix(" RETURNING *") {
        Some(head) => format!("{head} RETURNING rowid AS rowid, *"),
        None => sql.to_string(),
    }
}

/// Every `CHECK (…)` clause of a `CREATE TABLE` statement, named after the
/// preceding `CONSTRAINT name` when there is one and `check_N` otherwise.
/// String literals, quoted identifiers and comments are skipped, so a
/// parenthesis inside a literal never unbalances the scan.
pub(crate) fn check_constraints_from_create_sql(sql: &str) -> Vec<CheckConstraintSpec> {
    let bytes = sql.as_bytes();
    let mut out: Vec<CheckConstraintSpec> = Vec::new();
    let mut pending_name: Option<String> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(end) = skip_trivia_or_quoted(sql, i) {
            i = end;
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            let word = &sql[start..i];
            if word.eq_ignore_ascii_case("constraint") {
                let (name, next) = read_identifier(sql, i);
                pending_name = name;
                i = next;
                continue;
            }
            if word.eq_ignore_ascii_case("check") {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'(')
                    && let Some(end) = matching_paren(sql, j)
                {
                    let definition = sql[j + 1..end].trim().to_string();
                    let name = pending_name
                        .take()
                        .unwrap_or_else(|| format!("check_{}", out.len() + 1));
                    out.push(CheckConstraintSpec { name, definition });
                    i = end + 1;
                    continue;
                }
            }
            pending_name = None;
            continue;
        }
        i += 1;
    }
    out
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The identifier (bare or quoted) after position `i`, and where it ends.
fn read_identifier(sql: &str, i: usize) -> (Option<String>, usize) {
    let bytes = sql.as_bytes();
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return (None, j);
    }
    match bytes[j] {
        b'"' | b'`' | b'[' | b'\'' => {
            let end = skip_quoted(sql, j);
            let (open, close) = (j + 1, end.saturating_sub(1).max(j + 1));
            let raw = &sql[open..close];
            let unescaped = match bytes[j] {
                b'"' => raw.replace("\"\"", "\""),
                b'\'' => raw.replace("''", "'"),
                b'`' => raw.replace("``", "`"),
                _ => raw.to_string(),
            };
            (Some(unescaped), end)
        }
        c if is_ident_start(c) => {
            let start = j;
            while j < bytes.len() && is_ident_char(bytes[j]) {
                j += 1;
            }
            (Some(sql[start..j].to_string()), j)
        }
        _ => (None, j),
    }
}

/// When `i` sits on a comment or a quoted string/identifier, the index just
/// past it.
fn skip_trivia_or_quoted(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    match bytes[i] {
        b'\'' | b'"' | b'`' | b'[' => Some(skip_quoted(sql, i)),
        b'-' if bytes.get(i + 1) == Some(&b'-') => {
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            Some(j)
        }
        b'/' if bytes.get(i + 1) == Some(&b'*') => {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            Some((j + 2).min(bytes.len()))
        }
        _ => None,
    }
}

/// The index just past the quoted token starting at `i` (doubled quotes
/// stay inside the token; `[` closes with `]`).
fn skip_quoted(sql: &str, i: usize) -> usize {
    let bytes = sql.as_bytes();
    let open = bytes[i];
    let close = if open == b'[' { b']' } else { open };
    let mut j = i + 1;
    while j < bytes.len() {
        if bytes[j] == close {
            if open != b'[' && bytes.get(j + 1) == Some(&close) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    bytes.len()
}

/// The index of the `)` matching the `(` at `open`, skipping quoted text
/// and comments.
fn matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        if let Some(end) = skip_trivia_or_quoted(sql, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
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

/// What `pragma_table_list` knows about a name: its catalog spelling, kind,
/// and whether it is a `WITHOUT ROWID` table.
struct CatalogEntry {
    target: TableTarget,
    kind: String,
    without_rowid: bool,
}

pub(crate) struct LibSqlEngine {
    client: LibSqlClient,
    db: DatabaseType,
    /// The stream carrying the open transaction, between `begin` and
    /// `commit`/`rollback`; reads outside a transaction use a fresh
    /// one-request stream.
    stream: Option<LibSqlStream>,
}

impl LibSqlEngine {
    pub(crate) fn new(client: &LibSqlClient, db: DatabaseType) -> Self {
        Self {
            client: client.clone(),
            db,
            stream: None,
        }
    }

    async fn run(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<StatementOutcome> {
        match self.stream.as_mut() {
            Some(stream) => stream.execute(sql, params).await,
            None => {
                let mut stream = LibSqlStream::open(self.client.clone());
                stream.execute_closing(sql, params).await
            }
        }
    }

    async fn run_query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let outcome = self.run(sql, params).await?;
        Ok(MaterializedResultSet {
            columns: outcome.columns,
            rows: outcome.rows,
        })
    }

    async fn finish(&mut self, verb: &str) -> StudioResult<()> {
        let Some(mut stream) = self.stream.take() else {
            return Err(crate::row_editing::errors::libsql_no_transaction(verb));
        };
        stream.execute_closing(verb, &[]).await.map(|_| ())
    }

    /// The catalog entry for a (possibly differently cased) name.
    async fn catalog_entry(&mut self, target: &TableTarget) -> StudioResult<Option<CatalogEntry>> {
        let schema = if target.schema.is_empty() {
            "main"
        } else {
            target.schema.as_str()
        };
        let set = self
            .run_query(
                "SELECT schema, name, type, wr FROM main.pragma_table_list \
                 WHERE lower(schema) = lower(?1) AND lower(name) = lower(?2)",
                &[
                    SqlParam::String(schema.to_string()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        Ok(set.rows.first().and_then(|row| {
            Some(CatalogEntry {
                target: TableTarget {
                    schema: cell_str(row.first())?,
                    table: cell_str(row.get(1))?,
                },
                kind: cell_str(row.get(2)).unwrap_or_default(),
                without_rowid: cell_i64(row.get(3)).unwrap_or(0) != 0,
            })
        }))
    }

    /// Whether the table is addressed by its hidden rowid (a rowid table
    /// with no declared primary key and no column of that name).
    async fn rowid_keyed(&mut self, target: &TableTarget) -> StudioResult<bool> {
        let Some(entry) = self.catalog_entry(target).await? else {
            return Ok(false);
        };
        if entry.without_rowid || !entry.kind.eq_ignore_ascii_case("table") {
            return Ok(false);
        }
        let set = self
            .run_query(
                "SELECT count(*) FROM main.pragma_table_xinfo(?1, ?2) \
                 WHERE pk > 0 OR lower(name) IN ('rowid', '_rowid_', 'oid')",
                &[
                    SqlParam::String(entry.target.table),
                    SqlParam::String(entry.target.schema),
                ],
            )
            .await?;
        Ok(set
            .rows
            .first()
            .and_then(|row| cell_i64(row.first()))
            .unwrap_or(0)
            == 0)
    }

    /// Primary-key column names of a table, in key order.
    async fn table_key_columns(&mut self, target: &TableTarget) -> StudioResult<Vec<String>> {
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

    /// Best-effort display column for a referenced table: a text-ish
    /// declared column with a name-like identifier.
    async fn display_column_for(&mut self, target: &TableTarget) -> Option<String> {
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

    /// The table's CHECK constraints, read from its CREATE TABLE text.
    /// Best-effort: an unreadable catalog leaves the server as the sole
    /// authority (violations still surface on save).
    async fn check_constraints_for(&mut self, target: &TableTarget) -> Vec<CheckConstraintSpec> {
        let master = format!("{}.sqlite_master", self.db.quote_identifier(&target.schema));
        let Ok(set) = self
            .run_query(
                &format!("SELECT sql FROM {master} WHERE type = 'table' AND name = ?1"),
                &[SqlParam::String(target.table.clone())],
            )
            .await
        else {
            return Vec::new();
        };
        set.rows
            .first()
            .and_then(|row| cell_str(row.first()))
            .map(|sql| check_constraints_from_create_sql(&sql))
            .unwrap_or_default()
    }
}

#[async_trait]
impl RowEditEngine for LibSqlEngine {
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
        // 1. The server's view of the projection: prepare the statement
        //    inside a zero-row wrapper and read its column names.
        let probed = match self.run_query(&probe_sql(&statement), &[]).await {
            Ok(set) => set,
            Err(err) => {
                return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                    err,
                )));
            }
        };
        let labels: Vec<String> = probed.columns.iter().map(|c| c.name.clone()).collect();
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
        let Some(source) = single_source_table(&provenance, &labels, "main") else {
            return Ok(Ok(unsourced(&labels)));
        };
        // 3. Validate against the catalog: a base table, each traced column
        //    one of its columns spelled as stored (or its hidden rowid).
        let Some(entry) = self.catalog_entry(&source).await? else {
            return Ok(Ok(unsourced(&labels)));
        };
        if !entry.kind.eq_ignore_ascii_case("table") {
            return Ok(Err(crate::row_editing::not_editable::not_base_table(
                &dialect::quote_table(self.db, &entry.target),
            )));
        }
        let table = entry.target;
        let catalog: Vec<String> = self
            .run_query(
                "SELECT name FROM main.pragma_table_xinfo(?1, ?2) WHERE hidden <> 1",
                &[
                    SqlParam::String(table.table.clone()),
                    SqlParam::String(table.schema.clone()),
                ],
            )
            .await?
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect();
        let rowid_addressable = !entry.without_rowid && !catalog.iter().any(|c| is_rowid_name(c));
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
                            .or_else(|| {
                                (rowid_addressable && is_rowid_name(&origin.column))
                                    .then(|| ROWID_KEY.to_string())
                            })
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
        let Some(entry) = self.catalog_entry(target).await? else {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        };
        if !entry.kind.eq_ignore_ascii_case("table") {
            return Ok(Err(crate::row_editing::not_editable::not_base_table(
                &quoted,
            )));
        }
        let without_rowid = entry.without_rowid;
        let name_params = [
            SqlParam::String(target.table.clone()),
            SqlParam::String(target.schema.clone()),
        ];

        // Columns, including generated ones (hidden 2 = virtual, 3 = stored).
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
        let mut key_columns: Vec<String> = key_ordered.into_iter().map(|(_, name)| name).collect();

        if let Some(alias) = is_rowid_alias(&key_columns, &declared, without_rowid)
            && let Some(column) = columns.iter_mut().find(|c| c.name == alias)
        {
            column.is_identity = true;
        }

        // No declared key on a rowid table: the hidden rowid identifies rows.
        if key_columns.is_empty()
            && !without_rowid
            && !columns.iter().any(|c| is_rowid_name(&c.name))
        {
            columns.insert(
                0,
                RowEditColumn {
                    name: ROWID_KEY.to_string(),
                    data_type: "integer".to_string(),
                    char_max_length: None,
                    precision: None,
                    scale: None,
                    nullable: false,
                    is_primary_key: true,
                    is_identity: true,
                    is_computed: false,
                    is_rowversion: false,
                    default_definition: None,
                    result_ordinal: None,
                    enum_values: Vec::new(),
                },
            );
            key_columns = vec![ROWID_KEY.to_string()];
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

        let check_constraints = self.check_constraints_for(target).await;
        fill_enum_values_from_checks(&mut columns, &check_constraints);

        // Incoming foreign keys: sweep the schema's tables for references to
        // this one. Bounded by the table count; opens are user-paced.
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
        Ok(self.run(sql, params).await?.affected_rows)
    }

    async fn begin(&mut self) -> StudioResult<()> {
        if self.stream.is_some() {
            return Err(crate::row_editing::errors::libsql_transaction_already_open());
        }
        let mut stream = LibSqlStream::open(self.client.clone());
        stream.execute("BEGIN IMMEDIATE", &[]).await?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn commit(&mut self) -> StudioResult<()> {
        self.finish("COMMIT").await
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        self.finish("ROLLBACK").await
    }

    /// A rowid-keyed table's refetch projects the rowid the editor needs.
    async fn build_select_row(
        &mut self,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let (sql, params) = dialect::build_select_row_sql_for(self.db, target, key);
        if key.iter().any(|(column, _)| column == ROWID_KEY) {
            return Ok((with_rowid_projection(&sql), params));
        }
        Ok((sql, params))
    }

    /// `RETURNING *` never includes the hidden rowid; a rowid-keyed table
    /// returns it explicitly so the new row can be edited again.
    async fn insert_returning(
        &mut self,
        request: &InsertRowRequest,
    ) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
        let (mut sql, params) = dialect::build_insert_returning_sql_for(self.db, request);
        if self.rowid_keyed(&request.target).await? {
            sql = with_rowid_returning(&sql);
        }
        let set = self.query(&sql, &params).await?;
        expect_single_inserted_row(set)
    }

    /// The whole staged batch as one conditional Hrana batch on the open
    /// transaction's stream: the first failing statement stops the rest and
    /// surfaces its message, and the counts are checked before the caller
    /// commits.
    async fn run_batch_statements(
        &mut self,
        statements: &[StagedStatement],
    ) -> StudioResult<BatchRun> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(StudioError::QueryExecution(
                "no libSQL transaction is open for the batch".into(),
            ));
        };
        let steps: Vec<(String, Vec<SqlParam>)> = statements
            .iter()
            .map(|statement| (statement.sql.clone(), statement.params.clone()))
            .collect();
        let outcome = stream.batch(&steps).await?;
        if let Some(error) = outcome.error {
            return Err(StudioError::QueryExecution(error.message));
        }
        let affected: Vec<u64> = outcome
            .outcomes
            .iter()
            .map(|outcome| outcome.affected_rows)
            .collect();
        Ok(check_batch_counts(statements, &affected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_editing::staged::build_batch_statements;
    use crate::row_editing::{DeleteRowRequest, StagedBatch, StagedOp, UpdateRowRequest};
    use sqlly_engines::libsql::hrana::batch_request;

    #[test]
    fn single_statement_guard_strips_trailing_semicolons_and_refuses_scripts() {
        assert_eq!(
            single_statement("  SELECT id FROM t;  ").unwrap(),
            "SELECT id FROM t"
        );
        assert_eq!(
            single_statement("-- note\nSELECT 1 -- trailing").unwrap(),
            "SELECT 1 -- trailing"
        );
        assert!(
            single_statement("SELECT 1; SELECT 2")
                .unwrap_err()
                .contains("several")
        );
        assert!(
            single_statement("-- only a comment")
                .unwrap_err()
                .contains("empty")
        );
        assert_eq!(
            probe_sql("SELECT a FROM t ORDER BY a LIMIT 5"),
            "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 5) LIMIT 0"
        );
    }

    #[test]
    fn rowid_spellings_and_projections() {
        assert!(is_rowid_name("ROWID"));
        assert!(is_rowid_name("_rowid_"));
        assert!(is_rowid_name("oid"));
        assert!(!is_rowid_name("rowid2"));
        assert_eq!(
            with_rowid_projection("SELECT * FROM \"main\".\"t\" WHERE \"rowid\" = ?"),
            "SELECT rowid AS rowid, * FROM \"main\".\"t\" WHERE \"rowid\" = ?"
        );
        assert_eq!(
            with_rowid_returning("INSERT INTO \"t\" (\"v\") VALUES (?) RETURNING *"),
            "INSERT INTO \"t\" (\"v\") VALUES (?) RETURNING rowid AS rowid, *"
        );
        assert_eq!(
            with_rowid_returning("INSERT INTO \"t\" DEFAULT VALUES RETURNING *"),
            "INSERT INTO \"t\" DEFAULT VALUES RETURNING rowid AS rowid, *"
        );
        assert_eq!(
            with_rowid_projection("UPDATE t SET a = 1"),
            "UPDATE t SET a = 1"
        );
    }

    #[test]
    fn check_constraints_are_read_from_create_table_text() {
        let sql = "CREATE TABLE orders (\n\
                   id INTEGER PRIMARY KEY, -- check (ignored)\n\
                   status TEXT CHECK (status IN ('new', 'paid', 'a)b')),\n\
                   qty INT CONSTRAINT qty_positive CHECK(qty > 0 AND qty < 10),\n\
                   note TEXT DEFAULT '(check)',\n\
                   /* CHECK (nope) */ CONSTRAINT \"odd\"\"name\" CHECK ((qty % 2) = 1))";
        let checks = check_constraints_from_create_sql(sql);
        assert_eq!(checks.len(), 3, "{checks:?}");
        assert_eq!(checks[0].name, "check_1");
        assert_eq!(checks[0].definition, "status IN ('new', 'paid', 'a)b')");
        assert_eq!(checks[1].name, "qty_positive");
        assert_eq!(checks[1].definition, "qty > 0 AND qty < 10");
        assert_eq!(checks[2].name, "odd\"name");
        assert_eq!(checks[2].definition, "(qty % 2) = 1");
        assert!(check_constraints_from_create_sql("CREATE TABLE t (a INT)").is_empty());
        assert!(check_constraints_from_create_sql("CREATE TABLE t (a INT CHECK (a > 0").is_empty());

        // The extracted definition feeds the enum dropdown like the other
        // engines' catalog definitions do.
        let mut columns = vec![RowEditColumn {
            name: "status".into(),
            data_type: "text".into(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: true,
            is_primary_key: false,
            is_identity: false,
            is_computed: false,
            is_rowversion: false,
            default_definition: None,
            result_ordinal: None,
            enum_values: Vec::new(),
        }];
        fill_enum_values_from_checks(
            &mut columns,
            &check_constraints_from_create_sql(
                "CREATE TABLE t (status TEXT CHECK (status IN ('new', 'paid')))",
            ),
        );
        assert_eq!(
            columns[0].enum_values,
            vec!["new".to_string(), "paid".to_string()]
        );
    }

    /// The staged batch renders with SQLite's spellings and composes into
    /// one conditional Hrana batch: each step gated on the previous one,
    /// every value bound as a typed argument.
    #[test]
    fn staged_batch_composes_into_one_conditional_hrana_batch() {
        let target = TableTarget {
            schema: "main".into(),
            table: "person".into(),
        };
        let batch = StagedBatch {
            target: Some(target.clone()),
            key_columns: vec!["id".into()],
            ops: vec![
                StagedOp::Update(UpdateRowRequest {
                    target: target.clone(),
                    key: vec![("id".into(), CellValue::I64(1))],
                    rowversion: None,
                    changes: vec![("name".into(), CellValue::String("Bob".into()))],
                    original_values: vec![("name".into(), CellValue::String("Ada".into()))],
                    expressions: Vec::new(),
                }),
                StagedOp::Delete {
                    request: DeleteRowRequest {
                        target: target.clone(),
                        key: vec![("id".into(), CellValue::I64(2))],
                        rowversion: None,
                        original_values: vec![("note".into(), CellValue::Null)],
                    },
                    original_row: vec![
                        ("id".into(), CellValue::I64(2)),
                        ("note".into(), CellValue::Null),
                    ],
                },
            ],
        };
        // Deletes are ordered ahead of updates by the staging layer.
        let statements = build_batch_statements(DatabaseType::LibSql, &batch);
        assert_eq!(statements.len(), 2);
        assert_eq!(
            statements[0].sql,
            "DELETE FROM \"main\".\"person\" WHERE \"id\" = ? AND \"note\" IS NULL"
        );
        assert_eq!(
            statements[1].sql,
            "UPDATE \"main\".\"person\" SET \"name\" = ? WHERE \"id\" = ? AND \"name\" = ?"
        );
        assert_eq!(statements[0].expect_affected, Some(1));
        assert_eq!(statements[1].expect_affected, Some(1));

        let steps: Vec<(String, Vec<SqlParam>)> = statements
            .iter()
            .map(|s| (s.sql.clone(), s.params.clone()))
            .collect();
        let request = batch_request(&steps);
        let rendered = request["batch"]["steps"].as_array().unwrap();
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].get("condition").is_none());
        assert_eq!(rendered[1]["condition"]["type"], "ok");
        assert_eq!(rendered[1]["condition"]["step"], 0);
        assert_eq!(rendered[0]["stmt"]["args"].as_array().unwrap().len(), 1);
        assert_eq!(rendered[0]["stmt"]["args"][0]["value"], "2");
        assert_eq!(rendered[1]["stmt"]["args"][0]["type"], "text");
        assert_eq!(rendered[1]["stmt"]["args"][0]["value"], "Bob");
        assert_eq!(rendered[1]["stmt"]["args"][1]["value"], "1");
        assert_eq!(rendered[1]["stmt"]["args"][2]["value"], "Ada");

        // Counts are checked after the batch ran: a stale row is a conflict
        // at its own index, and a step the server skipped counts as 0.
        assert_eq!(check_batch_counts(&statements, &[1, 1]), BatchRun::Applied);
        assert_eq!(
            check_batch_counts(&statements, &[1, 0]),
            BatchRun::Mismatch {
                statement_index: 1,
                affected: 0
            }
        );
    }

    #[test]
    fn libsql_dml_uses_sqlite_spellings() {
        let insert = InsertRowRequest {
            target: TableTarget {
                schema: "main".into(),
                table: "t".into(),
            },
            values: vec![("v".into(), CellValue::String("x".into()))],
            expressions: Vec::new(),
        };
        let (sql, params) = dialect::build_insert_returning_sql_for(DatabaseType::LibSql, &insert);
        assert_eq!(
            sql,
            "INSERT INTO \"main\".\"t\" (\"v\") VALUES (?) RETURNING *"
        );
        assert_eq!(params, vec![SqlParam::String("x".into())]);
        assert_eq!(dialect::placeholder(DatabaseType::LibSql, 3), "?");
        assert_eq!(
            dialect::sql_literal_for(DatabaseType::LibSql, &CellValue::Bytes(vec![0xAB])),
            "X'AB'"
        );
    }
}
