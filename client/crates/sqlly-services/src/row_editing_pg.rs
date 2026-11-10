//! PostgreSQL row-editing engine.
//!
//! Provenance comes from the server's own description of the prepared
//! statement: `tokio-postgres` `prepare` exposes each result column's source
//! `table_oid`/`column_id`, which are resolved to schema-qualified names and
//! column names through `pg_catalog` — the PostgreSQL equivalent of
//! `sp_describe_first_result_set`. Table metadata, primary keys, foreign
//! keys, and CHECK constraints come from `pg_class`/`pg_attribute`/
//! `pg_index`/`pg_constraint`.
//!
//! PostgreSQL types parameters strictly, so this engine overrides the SQL
//! construction hooks to annotate every placeholder with a type: native
//! scalars bind as `$n::int8`/`$n::float8`/`$n::bool`/`$n::bytea`, and string
//! values destined for non-text columns go through `CAST($n::text AS <column
//! type>)` — PostgreSQL's I/O-conversion casts from `text` reach every type
//! (dates, uuids, numerics, enums, json). Symmetrically, columns whose types
//! the driver does not decode natively are projected as `CAST(col AS text)`,
//! so fetched originals and optimistic-concurrency comparisons round-trip
//! through the same representation.

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam, StudioResult};
use std::collections::HashMap;

use crate::row_editing::{
    CheckConstraintSpec, DeleteRowRequest, DescribeColumn, FkColumnRow, IncomingFkColumnRow,
    InsertRowRequest, LoadedTable, RelatedRowsRequest, RowEditColumn, TableTarget,
    UpdateRowRequest, fill_enum_values_from_checks, group_foreign_keys,
    group_incoming_foreign_keys,
};
use crate::row_editing_engines::{RowEditEngine, expect_single_inserted_row};
use crate::row_editing_sql as dialect;

/// One column of a table, as needed for typed SQL construction and catalog
/// metadata (a single `pg_catalog` query feeds both).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PgTableColumn {
    pub name: String,
    /// `pg_type.typname`, e.g. `int4`, `varchar`, `numeric`, `timestamptz`.
    pub type_name: String,
    /// The type's namespace, e.g. `pg_catalog` — cast targets are
    /// schema-qualified so user-defined types resolve unambiguously.
    pub type_schema: String,
    pub char_max_length: Option<i64>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
    pub nullable: bool,
    pub is_identity: bool,
    pub is_computed: bool,
    pub default_definition: Option<String>,
}

/// Types the driver decodes natively on the extended-protocol read path
/// (`typed_cell` in `sqlly-pg`); everything else is projected as text.
pub(crate) fn is_native_decode(type_name: &str) -> bool {
    matches!(
        type_name,
        "bool"
            | "int2"
            | "int4"
            | "int8"
            | "float4"
            | "float8"
            | "bytea"
            | "text"
            | "varchar"
            | "bpchar"
            | "name"
            | "uuid"
            | "date"
            | "time"
            | "timestamp"
            | "timestamptz"
    )
}

/// Types a bare `$n::text` parameter assigns/compares against without an
/// outer cast.
pub(crate) fn is_text_family(type_name: &str) -> bool {
    matches!(type_name, "text" | "varchar" | "bpchar" | "name" | "citext")
}

/// Schema-qualified cast target for a column's type.
fn cast_target(db: DatabaseType, column: &PgTableColumn) -> String {
    format!(
        "{}.{}",
        db.quote_identifier(&column.type_schema),
        db.quote_identifier(&column.type_name)
    )
}

/// Typed placeholder expression for parameter ordinal `n` carrying `value`
/// toward `column` (when the column's type is known).
pub(crate) fn param_expr(
    db: DatabaseType,
    n: usize,
    value: &CellValue,
    column: Option<&PgTableColumn>,
) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(_) => format!("${n}::bool"),
        CellValue::I64(_) => format!("${n}::int8"),
        CellValue::F64(_) => format!("${n}::float8"),
        CellValue::Bytes(_) => format!("${n}::bytea"),
        CellValue::String(_) => match column {
            Some(col) if !is_text_family(&col.type_name) => {
                format!("CAST(${n}::text AS {})", cast_target(db, col))
            }
            _ => format!("${n}::text"),
        },
    }
}

/// Projection item for one column: bare when the driver decodes the type
/// natively, `CAST(col AS text) AS col` otherwise.
pub(crate) fn projection_item(db: DatabaseType, column: &PgTableColumn) -> String {
    let quoted = db.quote_identifier(&column.name);
    if is_native_decode(&column.type_name) {
        quoted
    } else {
        format!("CAST({quoted} AS TEXT) AS {quoted}")
    }
}

/// Appends a typed `col = expr` / `col IS NULL` predicate.
fn push_typed_predicate(
    db: DatabaseType,
    predicates: &mut Vec<String>,
    params: &mut Vec<SqlParam>,
    column: &str,
    value: &CellValue,
    types: &[PgTableColumn],
) {
    if matches!(value, CellValue::Null) {
        predicates.push(format!("{} IS NULL", db.quote_identifier(column)));
    } else {
        params.push(SqlParam::from(value));
        let info = types.iter().find(|c| c.name == column);
        predicates.push(format!(
            "{} = {}",
            db.quote_identifier(column),
            param_expr(db, params.len(), value, info)
        ));
    }
}

pub(crate) struct PgEngine<'a> {
    client: &'a sqlly_engines::pg::PgClient,
    db: DatabaseType,
    /// Per-table column-type cache for this operation's lifetime.
    types: HashMap<(String, String), Vec<PgTableColumn>>,
}

impl<'a> PgEngine<'a> {
    pub(crate) fn new(client: &'a sqlly_engines::pg::PgClient, db: DatabaseType) -> Self {
        Self {
            client,
            db,
            types: HashMap::new(),
        }
    }

    async fn run_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet> {
        let sets = sqlly_engines::pg::query_rows_on_client(self.client, sql, params).await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    /// Column types of a table, ordered by `attnum`, cached per operation.
    async fn table_types(&mut self, target: &TableTarget) -> StudioResult<Vec<PgTableColumn>> {
        let key = (target.schema.clone(), target.table.clone());
        if let Some(cached) = self.types.get(&key) {
            return Ok(cached.clone());
        }
        let set = self
            .run_query(
                "SELECT a.attname, t.typname, tn.nspname, \
                 CASE WHEN t.typname IN ('varchar','bpchar') AND a.atttypmod >= 4 \
                 THEN (a.atttypmod - 4)::int8 ELSE NULL END AS char_len, \
                 CASE WHEN t.typname = 'numeric' AND a.atttypmod >= 4 \
                 THEN (((a.atttypmod - 4) >> 16) & 65535)::int8 ELSE NULL END AS num_precision, \
                 CASE WHEN t.typname = 'numeric' AND a.atttypmod >= 4 \
                 THEN ((a.atttypmod - 4) & 65535)::int8 ELSE NULL END AS num_scale, \
                 (NOT a.attnotnull) AS nullable, \
                 (a.attidentity <> '') AS is_identity, \
                 (a.attgenerated <> '') AS is_computed, \
                 pg_get_expr(d.adbin, d.adrelid) AS default_definition \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_namespace tn ON tn.oid = t.typnamespace \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[
                    SqlParam::String(target.schema.clone()),
                    SqlParam::String(target.table.clone()),
                ],
            )
            .await?;
        let columns: Vec<PgTableColumn> = set
            .rows
            .iter()
            .filter_map(|row| {
                Some(PgTableColumn {
                    name: cell_str(row.first())?,
                    type_name: cell_str(row.get(1)).unwrap_or_default(),
                    type_schema: cell_str(row.get(2)).unwrap_or_default(),
                    char_max_length: cell_i64(row.get(3)),
                    precision: cell_i64(row.get(4)).map(|p| p as u8),
                    scale: cell_i64(row.get(5)).map(|s| s as u8),
                    nullable: cell_bool(row.get(6)),
                    is_identity: cell_bool(row.get(7)),
                    is_computed: cell_bool(row.get(8)),
                    default_definition: cell_str(row.get(9)),
                })
            })
            .collect();
        self.types.insert(key, columns.clone());
        Ok(columns)
    }

    /// `SELECT <typed projection> FROM table` list, or `*` when the table's
    /// types are unavailable.
    async fn projection_list(&mut self, target: &TableTarget) -> StudioResult<String> {
        let columns = self.table_types(target).await?;
        if columns.is_empty() {
            return Ok("*".to_string());
        }
        Ok(columns
            .iter()
            .map(|c| projection_item(self.db, c))
            .collect::<Vec<_>>()
            .join(", "))
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

#[async_trait]
impl RowEditEngine for PgEngine<'_> {
    fn database_type(&self) -> DatabaseType {
        self.db
    }

    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, sqlly_core::DisplayMessage>> {
        let origins = match sqlly_engines::pg::describe_statement_on_client(self.client, sql).await
        {
            Ok(origins) => origins,
            Err(err) => {
                return Ok(Err(crate::row_editing::not_editable::provenance_failed(
                    err,
                )));
            }
        };

        // Resolve each distinct source table's schema-qualified name and its
        // attribute-number → column-name map. OIDs are numbers straight from
        // the server, safe to inline.
        let mut tables: HashMap<u32, (String, String)> = HashMap::new();
        let mut attnames: HashMap<u32, HashMap<i64, String>> = HashMap::new();
        for origin in &origins {
            let Some(oid) = origin.table_oid else {
                continue;
            };
            if tables.contains_key(&oid) {
                continue;
            }
            let set = self
                .run_query(
                    &format!(
                        "SELECT n.nspname, c.relname FROM pg_class c \
                         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = {oid}"
                    ),
                    &[],
                )
                .await?;
            let Some(row) = set.rows.first() else {
                continue;
            };
            let (Some(schema), Some(table)) = (cell_str(row.first()), cell_str(row.get(1))) else {
                continue;
            };
            tables.insert(oid, (schema, table));
            let set = self
                .run_query(
                    &format!(
                        "SELECT a.attnum, a.attname FROM pg_attribute a \
                         WHERE a.attrelid = {oid} AND a.attnum > 0 AND NOT a.attisdropped"
                    ),
                    &[],
                )
                .await?;
            let map: HashMap<i64, String> = set
                .rows
                .iter()
                .filter_map(|row| Some((cell_i64(row.first())?, cell_str(row.get(1))?)))
                .collect();
            attnames.insert(oid, map);
        }

        Ok(Ok(origins
            .iter()
            .map(|origin| {
                let source = origin.table_oid.and_then(|oid| {
                    let (schema, table) = tables.get(&oid)?;
                    let column = attnames
                        .get(&oid)?
                        .get(&i64::from(origin.column_id?))?
                        .clone();
                    Some((schema.clone(), table.clone(), column))
                });
                DescribeColumn {
                    name: origin.name.clone(),
                    is_hidden: false,
                    source_schema: source.as_ref().map(|(s, _, _)| s.clone()),
                    source_table: source.as_ref().map(|(_, t, _)| t.clone()),
                    source_column: source.map(|(_, _, c)| c),
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

        // Base-table check: only ordinary/partitioned tables are editable.
        let set = self
            .run_query(
                "SELECT (c.relkind IN ('r','p')) AS is_base FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &name_params,
            )
            .await?;
        let Some(row) = set.rows.first() else {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        };
        if !cell_bool(row.first()) {
            return Ok(Err(crate::row_editing::not_editable::not_base_table(
                &quoted,
            )));
        }

        let typed = self.table_types(target).await?;
        if typed.is_empty() {
            return Ok(Err(crate::row_editing::not_editable::table_not_found(
                &quoted,
            )));
        }

        let key_columns: Vec<String> = self
            .run_query(
                "SELECT a.attname FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND i.indisprimary \
                 ORDER BY k.ord",
                &name_params,
            )
            .await?
            .rows
            .iter()
            .filter_map(|row| cell_str(row.first()))
            .collect();

        let mut enum_map: HashMap<String, Vec<String>> = HashMap::new();
        let enum_set = self
            .run_query(
                "SELECT a.attname, e.enumlabel FROM pg_attribute a \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_enum e ON e.enumtypid = t.oid \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND t.typtype = 'e' \
                 ORDER BY a.attnum, e.enumsortorder",
                &name_params,
            )
            .await?;
        for row in &enum_set.rows {
            let Some(name) = cell_str(row.first()) else {
                continue;
            };
            let Some(label) = cell_str(row.get(1)) else {
                continue;
            };
            enum_map.entry(name).or_default().push(label);
        }

        let mut columns: Vec<RowEditColumn> = typed
            .iter()
            .map(|c| RowEditColumn {
                name: c.name.clone(),
                data_type: c.type_name.clone(),
                char_max_length: c.char_max_length,
                precision: c.precision,
                scale: c.scale,
                nullable: c.nullable,
                is_primary_key: key_columns.contains(&c.name),
                is_identity: c.is_identity,
                is_computed: c.is_computed,
                is_rowversion: false,
                default_definition: c.default_definition.clone(),
                result_ordinal: None,
                enum_values: enum_map.get(&c.name).cloned().unwrap_or_default(),
            })
            .collect();

        // Outgoing foreign keys, with column pairs in constraint key order.
        let fk_set = self
            .run_query(
                "SELECT con.conname, rns.nspname AS ref_schema, rc.relname AS ref_table, \
                 la.attname AS parent_column, ra.attname AS ref_column, k.ord \
                 FROM pg_constraint con \
                 JOIN pg_class pc ON pc.oid = con.conrelid \
                 JOIN pg_namespace pns ON pns.oid = pc.relnamespace \
                 JOIN pg_class rc ON rc.oid = con.confrelid \
                 JOIN pg_namespace rns ON rns.oid = rc.relnamespace \
                 CROSS JOIN LATERAL unnest(con.conkey, con.confkey) \
                 WITH ORDINALITY AS k(child_att, parent_att, ord) \
                 JOIN pg_attribute la ON la.attrelid = con.conrelid AND la.attnum = k.child_att \
                 JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = k.parent_att \
                 WHERE pns.nspname = $1 AND pc.relname = $2 AND con.contype = 'f' \
                 ORDER BY con.conname, k.ord",
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
        // Resolve one display column per referenced table (best-effort).
        let mut display_cache: HashMap<(String, String), Option<String>> = HashMap::new();
        for row in &mut fk_rows {
            let key = (row.ref_schema.clone(), row.ref_table.clone());
            if !display_cache.contains_key(&key) {
                let display = self
                    .run_query(
                        "SELECT a.attname FROM pg_attribute a \
                         JOIN pg_class c ON c.oid = a.attrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         JOIN pg_type t ON t.oid = a.atttypid \
                         WHERE n.nspname = $1 AND c.relname = $2 \
                         AND a.attnum > 0 AND NOT a.attisdropped \
                         AND t.typname IN ('text','varchar','bpchar','name','citext') \
                         AND lower(a.attname) IN \
                         ('name','displayname','fullname','title','label','description') \
                         ORDER BY CASE lower(a.attname) WHEN 'name' THEN 0 \
                         WHEN 'displayname' THEN 1 WHEN 'fullname' THEN 2 WHEN 'title' THEN 3 \
                         WHEN 'label' THEN 4 WHEN 'description' THEN 5 ELSE 6 END, a.attnum \
                         LIMIT 1",
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

        let check_constraints: Vec<CheckConstraintSpec> = self
            .run_query(
                "SELECT con.conname, pg_get_constraintdef(con.oid) FROM pg_constraint con \
                 JOIN pg_class c ON c.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND con.contype = 'c' \
                 ORDER BY con.conname",
                &name_params,
            )
            .await?
            .rows
            .iter()
            .filter_map(|row| {
                Some(CheckConstraintSpec {
                    name: cell_str(row.first())?,
                    definition: cell_str(row.get(1))?,
                })
            })
            .collect();

        fill_enum_values_from_checks(&mut columns, &check_constraints);

        let incoming_set = self
            .run_query(
                "SELECT con.conname, cns.nspname AS child_schema, cc.relname AS child_table, \
                 ca.attname AS child_column, ta.attname AS this_column, k.ord \
                 FROM pg_constraint con \
                 JOIN pg_class cc ON cc.oid = con.conrelid \
                 JOIN pg_namespace cns ON cns.oid = cc.relnamespace \
                 JOIN pg_class tc ON tc.oid = con.confrelid \
                 JOIN pg_namespace tns ON tns.oid = tc.relnamespace \
                 CROSS JOIN LATERAL unnest(con.conkey, con.confkey) \
                 WITH ORDINALITY AS k(child_att, this_att, ord) \
                 JOIN pg_attribute ca ON ca.attrelid = con.conrelid AND ca.attnum = k.child_att \
                 JOIN pg_attribute ta ON ta.attrelid = con.confrelid AND ta.attnum = k.this_att \
                 WHERE tns.nspname = $1 AND tc.relname = $2 AND con.contype = 'f' \
                 ORDER BY con.conname, k.ord",
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
        sqlly_engines::pg::execute_params_on_client(self.client, sql, params).await
    }

    async fn begin(&mut self) -> StudioResult<()> {
        sqlly_engines::pg::execute_batch_on_client(self.client, "BEGIN").await
    }

    async fn commit(&mut self) -> StudioResult<()> {
        sqlly_engines::pg::execute_batch_on_client(self.client, "COMMIT").await
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        sqlly_engines::pg::execute_batch_on_client(self.client, "ROLLBACK").await
    }

    async fn build_select_row(
        &mut self,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(target).await?;
        let projection = self.projection_list(target).await?;
        let mut params: Vec<SqlParam> = Vec::new();
        let mut predicates: Vec<String> = Vec::new();
        for (column, value) in key {
            push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
        }
        let sql = format!(
            "SELECT {projection} FROM {} WHERE {}",
            dialect::quote_table(self.db, target),
            predicates.join(" AND ")
        );
        Ok((sql, params))
    }

    async fn build_update(
        &mut self,
        request: &UpdateRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(&request.target).await?;
        let mut params: Vec<SqlParam> = Vec::new();
        let mut assignments: Vec<String> = Vec::new();
        for (column, value) in &request.changes {
            if matches!(value, CellValue::Null) {
                assignments.push(format!("{} = NULL", self.db.quote_identifier(column)));
            } else {
                params.push(SqlParam::from(value));
                let info = types.iter().find(|c| &c.name == column);
                assignments.push(format!(
                    "{} = {}",
                    self.db.quote_identifier(column),
                    param_expr(self.db, params.len(), value, info)
                ));
            }
        }
        let mut predicates: Vec<String> = Vec::new();
        for (column, value) in &request.key {
            push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
        }
        match &request.rowversion {
            Some((column, value)) => {
                push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
            }
            None => {
                for (column, value) in &request.original_values {
                    push_typed_predicate(
                        self.db,
                        &mut predicates,
                        &mut params,
                        column,
                        value,
                        &types,
                    );
                }
            }
        }
        let sql = format!(
            "UPDATE {} SET {} WHERE {}",
            dialect::quote_table(self.db, &request.target),
            assignments.join(", "),
            predicates.join(" AND ")
        );
        Ok((sql, params))
    }

    async fn build_delete(
        &mut self,
        request: &DeleteRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(&request.target).await?;
        let mut params: Vec<SqlParam> = Vec::new();
        let mut predicates: Vec<String> = Vec::new();
        for (column, value) in &request.key {
            push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
        }
        match &request.rowversion {
            Some((column, value)) => {
                push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
            }
            None => {
                for (column, value) in &request.original_values {
                    push_typed_predicate(
                        self.db,
                        &mut predicates,
                        &mut params,
                        column,
                        value,
                        &types,
                    );
                }
            }
        }
        let sql = format!(
            "DELETE FROM {} WHERE {}",
            dialect::quote_table(self.db, &request.target),
            predicates.join(" AND ")
        );
        Ok((sql, params))
    }

    async fn build_related(
        &mut self,
        request: &RelatedRowsRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(&request.table).await?;
        let projection = self.projection_list(&request.table).await?;
        let limit = request.limit.max(1);
        let mut params: Vec<SqlParam> = Vec::new();
        let mut predicates: Vec<String> = Vec::new();
        for (column, value) in &request.predicates {
            push_typed_predicate(self.db, &mut predicates, &mut params, column, value, &types);
        }
        let where_clause = if predicates.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", predicates.join(" AND "))
        };
        let sql = format!(
            "SELECT {projection} FROM {}{} LIMIT {}",
            dialect::quote_table(self.db, &request.table),
            where_clause,
            limit + 1
        );
        Ok((sql, params))
    }

    async fn build_fk_display_lookup(
        &mut self,
        referenced: &TableTarget,
        display: &str,
        ref_column: &str,
        value: &CellValue,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        let types = self.table_types(referenced).await?;
        let mut params: Vec<SqlParam> = Vec::new();
        let mut predicates: Vec<String> = Vec::new();
        push_typed_predicate(
            self.db,
            &mut predicates,
            &mut params,
            ref_column,
            value,
            &types,
        );
        let sql = format!(
            "SELECT {} FROM {} WHERE {} LIMIT 1",
            self.db.quote_identifier(display),
            dialect::quote_table(self.db, referenced),
            predicates.join(" AND ")
        );
        Ok((sql, params))
    }

    async fn insert_returning(
        &mut self,
        request: &InsertRowRequest,
    ) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
        let types = self.table_types(&request.target).await?;
        let returning = self.projection_list(&request.target).await?;
        let table = dialect::quote_table(self.db, &request.target);
        let (sql, params) = if request.values.is_empty() {
            (
                format!("INSERT INTO {table} DEFAULT VALUES RETURNING {returning}"),
                Vec::new(),
            )
        } else {
            let mut params: Vec<SqlParam> = Vec::new();
            let mut columns: Vec<String> = Vec::with_capacity(request.values.len());
            let mut exprs: Vec<String> = Vec::with_capacity(request.values.len());
            for (column, value) in &request.values {
                columns.push(self.db.quote_identifier(column));
                if matches!(value, CellValue::Null) {
                    exprs.push("NULL".to_string());
                } else {
                    params.push(SqlParam::from(value));
                    let info = types.iter().find(|c| &c.name == column);
                    exprs.push(param_expr(self.db, params.len(), value, info));
                }
            }
            (
                format!(
                    "INSERT INTO {table} ({}) VALUES ({}) RETURNING {returning}",
                    columns.join(", "),
                    exprs.join(", ")
                ),
                params,
            )
        };
        let set = self.run_query(&sql, &params).await?;
        expect_single_inserted_row(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, type_name: &str) -> PgTableColumn {
        PgTableColumn {
            name: name.to_string(),
            type_name: type_name.to_string(),
            type_schema: "pg_catalog".to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: true,
            is_identity: false,
            is_computed: false,
            default_definition: None,
        }
    }

    #[test]
    fn native_and_text_family_classification() {
        assert!(is_native_decode("int4"));
        assert!(is_native_decode("timestamptz"));
        assert!(is_native_decode("uuid"));
        assert!(!is_native_decode("numeric"));
        assert!(!is_native_decode("jsonb"));
        assert!(is_text_family("varchar"));
        assert!(is_text_family("citext"));
        assert!(!is_text_family("numeric"));
    }

    #[test]
    fn param_exprs_annotate_native_scalars() {
        let db = DatabaseType::Postgres;
        assert_eq!(param_expr(db, 1, &CellValue::I64(5), None), "$1::int8");
        assert_eq!(param_expr(db, 2, &CellValue::F64(1.5), None), "$2::float8");
        assert_eq!(param_expr(db, 3, &CellValue::Bool(true), None), "$3::bool");
        assert_eq!(
            param_expr(db, 4, &CellValue::Bytes(vec![1]), None),
            "$4::bytea"
        );
    }

    #[test]
    fn string_params_cast_through_text_to_the_column_type() {
        let db = DatabaseType::Postgres;
        let numeric = column("amount", "numeric");
        assert_eq!(
            param_expr(db, 1, &CellValue::String("1.50".into()), Some(&numeric)),
            "CAST($1::text AS \"pg_catalog\".\"numeric\")"
        );
        let text = column("name", "varchar");
        assert_eq!(
            param_expr(db, 2, &CellValue::String("x".into()), Some(&text)),
            "$2::text"
        );
        // Unknown column: stay text and let the server decide.
        assert_eq!(
            param_expr(db, 3, &CellValue::String("x".into()), None),
            "$3::text"
        );
    }

    #[test]
    fn projection_casts_only_non_native_types() {
        let db = DatabaseType::Postgres;
        assert_eq!(projection_item(db, &column("id", "int4")), "\"id\"");
        assert_eq!(
            projection_item(db, &column("amount", "numeric")),
            "CAST(\"amount\" AS TEXT) AS \"amount\""
        );
        assert_eq!(
            projection_item(db, &column("doc", "jsonb")),
            "CAST(\"doc\" AS TEXT) AS \"doc\""
        );
    }

    fn live_pg_profile() -> Option<sqlly_core::ConnectionProfile> {
        use sqlly_core::{ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
        let server = std::env::var("SQLLY_PG_SERVER").ok()?;
        let user = std::env::var("SQLLY_PG_USER").ok()?;
        let mut profile = ConnectionProfile::sql_login(
            "pg-rowedit-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PG_PASSWORD".into(),
            },
        );
        profile.port = std::env::var("SQLLY_PG_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);
        profile.database = std::env::var("SQLLY_PG_DATABASE").ok();
        profile.database_type = DatabaseType::Postgres;
        profile.encrypt = false;
        Some(profile)
    }

    /// Live lifecycle test: set SQLLY_PG_SERVER / SQLLY_PG_USER /
    /// SQLLY_PG_PASSWORD (and optionally SQLLY_PG_DATABASE, SQLLY_PG_PORT).
    /// Skips silently when no live server is configured.
    #[tokio::test]
    async fn live_pg_row_edit_lifecycle() {
        use crate::row_editing::{
            DefaultRowEditingService, DeleteRowOutcome, DeleteRowRequest, InsertRowOutcome,
            InsertRowRequest, RowEditOpenOutcome, RowEditOpenRequest, RowEditingService,
            UpdateRowOutcome, UpdateRowRequest,
        };

        let Some(profile) = live_pg_profile() else {
            eprintln!("skipping: SQLLY_PG_* env vars not set");
            return;
        };

        let client = sqlly_engines::pg::open_client(&profile).await.unwrap();
        sqlly_engines::pg::execute_batch_on_client(
            &client,
            "DROP TABLE IF EXISTS sqlly_rowedit_live; \
             CREATE TABLE sqlly_rowedit_live (\
             id serial PRIMARY KEY, name text NOT NULL, amount numeric(10,2), \
             created timestamptz NOT NULL DEFAULT now()); \
             INSERT INTO sqlly_rowedit_live (name, amount) \
             VALUES ('Ada', 1.50), ('Grace', 2.25);",
        )
        .await
        .unwrap();

        let service = DefaultRowEditingService::default();
        let sql = "SELECT id, name, amount FROM sqlly_rowedit_live ORDER BY id";
        let sets = sqlly_engines::pg::query_rows_on_client(&client, sql, &[])
            .await
            .unwrap();
        let set = sets.into_iter().next().unwrap();
        let request = RowEditOpenRequest {
            sql: sql.to_string(),
            result_set_index: 0,
            columns: set.columns,
            row: set.rows[0].clone(),
        };

        // Open: provenance via prepared-statement column origins; the
        // numeric column round-trips as text through the cast projection.
        let session = match service.open_session(&profile, request).await.unwrap() {
            RowEditOpenOutcome::Editable(session) => *session,
            RowEditOpenOutcome::NotEditable { reason, .. } => panic!("not editable: {reason}"),
        };
        assert_eq!(session.target.table, "sqlly_rowedit_live");
        assert_eq!(session.key_columns, vec!["id"]);
        assert_eq!(session.original_row[2], CellValue::String("1.50".into()));

        // Update a numeric column from its text draft (CAST($n::text AS
        // numeric) both in SET and in the concurrency WHERE).
        let update = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("amount".into(), CellValue::String("9.99".into()))],
            original_values: vec![("amount".into(), CellValue::String("1.50".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&profile, update.clone()).await.unwrap() {
            UpdateRowOutcome::Updated { columns, row } => {
                let at = columns.iter().position(|c| c == "amount").unwrap();
                assert_eq!(row[at], CellValue::String("9.99".into()));
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        // Same request again: stale originals now conflict.
        assert!(matches!(
            service.update_row(&profile, update).await.unwrap(),
            UpdateRowOutcome::Conflict { .. }
        ));

        // Insert resolves the serial key and defaults via RETURNING.
        let inserted = service
            .insert_row(
                &profile,
                InsertRowRequest {
                    target: session.target.clone(),
                    values: vec![
                        ("name".into(), CellValue::String("Zed".into())),
                        ("amount".into(), CellValue::String("3.00".into())),
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
        assert!(new_id > 2);

        // Guarded delete, then conflict on the second attempt.
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

        sqlly_engines::pg::execute_batch_on_client(&client, "DROP TABLE sqlly_rowedit_live")
            .await
            .unwrap();
    }
}
