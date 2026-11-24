//! Materializing rows into a workspace relation.
//!
//! Rows stream through DuckDB's appender into a temporary staging table
//! whose columns are VARCHAR (BLOB for binary columns), so no value can fail
//! the append itself: an appender that rejects a value is dead, and the
//! whole ingest with it. Once the rows are in, one scan checks which mapped
//! types every value converts to (and sizes undeclared decimals), and the
//! relation is created from the staging table with those types, falling a
//! column back to VARCHAR when its mapped type does not fit every value.
//! All of it runs in one transaction: the relation appears complete (or
//! partial, at a quota) or not at all.

use super::{
    CancelToken, LocalColumn, LocalResult, LocalStoreError, LocalType, MaterializeOutcome,
    PartialReason, ProvenanceRecord, SourceInfo, WorkspaceQuota, WriteMode, drop_relation_sql, hex,
    in_transaction, local_type_for, now_rfc3339, quote_ident, relation_kind, write_provenance,
};
use ::duckdb::types::Value;
use sha2::{Digest, Sha256};
use sqlly_core::{CellValue, ResultColumn};

/// One batch of rows from the source, or the error that ended it.
pub type SourceBatch = Result<Vec<Vec<CellValue>>, String>;

/// What to materialize and where it came from.
#[derive(Debug, Clone)]
pub struct MaterializeRequest<'a> {
    pub relation: &'a str,
    pub columns: &'a [ResultColumn],
    pub source: &'a SourceInfo,
    pub mode: WriteMode,
}

/// How often (in rows) ingestion looks at the cancel token.
const CANCEL_CHECK_ROWS: u64 = 4096;

/// The row and byte budget for one relation.
#[derive(Debug, Clone)]
pub(super) struct Limits {
    pub max_rows: u64,
    pub max_bytes: u64,
    /// The byte budget is the disk quota's remaining room rather than the
    /// per-relation limit.
    disk_bound: bool,
}

impl Limits {
    pub fn new(quota: &WorkspaceQuota, disk_left: u64) -> Self {
        Self {
            max_rows: quota.max_rows_per_relation,
            max_bytes: quota.max_bytes_per_relation.min(disk_left),
            disk_bound: disk_left < quota.max_bytes_per_relation,
        }
    }

    pub fn byte_partial(&self) -> PartialReason {
        if self.disk_bound {
            PartialReason::DiskQuota {
                limit: self.max_bytes,
            }
        } else {
            PartialReason::ByteLimit {
                limit: self.max_bytes,
            }
        }
    }
}

/// The file an import read, for its provenance.
#[derive(Debug, Clone)]
pub(super) struct FileOrigin {
    pub path: String,
    pub sha256: String,
}

struct PlannedColumn {
    name: String,
    source_type: String,
    mapped: LocalType,
    staging: String,
}

/// Output column names: blanks get `column<N>`, and repeats (compared
/// case-insensitively, as DuckDB does) get `_2`, `_3`, ….
pub(super) fn unique_column_names(columns: &[ResultColumn]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::with_capacity(columns.len());
    let mut out = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let base = match column.name.trim() {
            "" => format!("column{}", index + 1),
            name => name.to_string(),
        };
        let mut name = base.clone();
        let mut suffix = 2;
        while seen.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        seen.push(name.clone());
        out.push(name);
    }
    out
}

fn plan(request: &MaterializeRequest<'_>) -> Vec<PlannedColumn> {
    unique_column_names(request.columns)
        .into_iter()
        .zip(request.columns)
        .enumerate()
        .map(|(index, (name, column))| PlannedColumn {
            name,
            source_type: column.data_type.clone(),
            mapped: local_type_for(request.source.engine, &column.data_type),
            staging: format!("c{index}"),
        })
        .collect()
}

/// Refuse or clear the way for `relation` according to `mode`.
pub(super) fn prepare_target(
    conn: &::duckdb::Connection,
    relation: &str,
    mode: WriteMode,
) -> LocalResult<()> {
    if let Some(is_view) = relation_kind(conn, relation)? {
        match mode {
            WriteMode::FailIfExists => {
                return Err(LocalStoreError::RelationExists(relation.to_string()));
            }
            WriteMode::Replace => drop_relation_sql(conn, relation, is_view)?,
        }
    }
    Ok(())
}

pub(super) fn materialize(
    conn: &::duckdb::Connection,
    request: &MaterializeRequest<'_>,
    batches: impl Iterator<Item = SourceBatch>,
    limits: &Limits,
    cancel: &CancelToken,
) -> LocalResult<MaterializeOutcome> {
    materialize_with_origin(conn, request, batches, limits, cancel, None)
}

pub(super) fn materialize_with_origin(
    conn: &::duckdb::Connection,
    request: &MaterializeRequest<'_>,
    batches: impl Iterator<Item = SourceBatch>,
    limits: &Limits,
    cancel: &CancelToken,
    origin: Option<&FileOrigin>,
) -> LocalResult<MaterializeOutcome> {
    if request.columns.is_empty() {
        return Err(LocalStoreError::Source(
            "the result has no columns to materialize".to_string(),
        ));
    }
    let columns = plan(request);
    let staging = format!("_sqlly_staging_{}", uuid::Uuid::new_v4().simple());
    in_transaction(conn, |conn| {
        prepare_target(conn, request.relation, request.mode)?;
        let ddl = columns
            .iter()
            .map(|c| {
                let carrier = if c.mapped.is_blob() {
                    "BLOB"
                } else {
                    "VARCHAR"
                };
                format!("{} {carrier}", quote_ident(&c.staging))
            })
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE {} ({ddl})",
            quote_ident(&staging)
        ))?;
        let appended = append_rows(conn, &staging, &columns, batches, limits, cancel)?;
        cancel.check()?;
        let resolved = resolve_types(conn, &staging, &columns)?;
        let select = columns
            .iter()
            .zip(&resolved)
            .map(|(column, local)| {
                let staged = quote_ident(&column.staging);
                let value = match local {
                    LocalType::Varchar | LocalType::Blob => staged,
                    other => format!("TRY_CAST({staged} AS {})", other.sql()),
                };
                format!("{value} AS {}", quote_ident(&column.name))
            })
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "CREATE TABLE {} AS SELECT {select} FROM temp.main.{}; DROP TABLE temp.main.{};",
            quote_ident(request.relation),
            quote_ident(&staging),
            quote_ident(&staging),
        ))?;

        let local_columns: Vec<LocalColumn> = columns
            .iter()
            .zip(&resolved)
            .map(|(column, local)| LocalColumn {
                name: column.name.clone(),
                source_type: column.source_type.clone(),
                local_type: local.sql(),
                fell_back_to_text: *local == LocalType::Varchar
                    && !matches!(column.mapped, LocalType::Varchar | LocalType::Infer),
            })
            .collect();
        let partial = appended.partial.or_else(|| {
            request
                .source
                .source_incomplete
                .clone()
                .map(|detail| PartialReason::SourceIncomplete { detail })
        });
        let checksum = match origin {
            Some(file) => Some(format!("file-sha256:{}", file.sha256)),
            None => Some(format!("rows-sha256:{}", appended.checksum)),
        };
        let record = provenance_record(
            request,
            origin,
            appended.rows,
            appended.bytes,
            &local_columns,
            partial.clone(),
            checksum.clone(),
        );
        write_provenance(conn, &record)?;
        Ok(MaterializeOutcome {
            relation: request.relation.to_string(),
            row_count: appended.rows,
            byte_count: appended.bytes,
            columns: local_columns,
            partial,
            checksum,
        })
    })
}

pub(super) fn provenance_record(
    request: &MaterializeRequest<'_>,
    origin: Option<&FileOrigin>,
    rows: u64,
    bytes: u64,
    columns: &[LocalColumn],
    partial: Option<PartialReason>,
    checksum: Option<String>,
) -> ProvenanceRecord {
    let source = request.source;
    let now = now_rfc3339();
    ProvenanceRecord {
        relation: request.relation.to_string(),
        source_kind: source.kind,
        source_label: source.label.clone(),
        engine: serde_json::to_value(source.engine)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string)),
        profile_id: source.profile_id.clone(),
        database: source.database.clone(),
        environment: source.environment.clone(),
        source_sql_hash: source.sql_hash.clone(),
        source_path: origin.map(|file| file.path.clone()),
        cell_id: source.cell_id.clone(),
        run_id: source.run_id.clone(),
        upstream: source.upstream.clone(),
        extracted_at: source.extracted_at.clone().unwrap_or_else(|| now.clone()),
        materialized_at: now,
        row_count: rows,
        byte_count: bytes,
        columns: columns.to_vec(),
        complete: partial.is_none(),
        partial_reason: partial,
        checksum,
    }
}

struct Appended {
    rows: u64,
    bytes: u64,
    partial: Option<PartialReason>,
    checksum: String,
}

/// The bytes a value counts against the byte quota.
fn cell_bytes(cell: &CellValue) -> u64 {
    match cell {
        CellValue::Null => 0,
        CellValue::Bool(_) => 1,
        CellValue::I64(_) | CellValue::F64(_) => 8,
        CellValue::String(s) => s.len() as u64,
        CellValue::Bytes(b) => b.len() as u64,
    }
}

fn append_rows(
    conn: &::duckdb::Connection,
    staging: &str,
    columns: &[PlannedColumn],
    batches: impl Iterator<Item = SourceBatch>,
    limits: &Limits,
    cancel: &CancelToken,
) -> LocalResult<Appended> {
    let mut appender = conn.appender_to_catalog_and_db(staging, "temp", "main")?;
    let mut hasher = Sha256::new();
    let mut rows = 0u64;
    let mut bytes = 0u64;
    let mut partial = None;
    'batches: for batch in batches {
        cancel.check()?;
        let batch = batch.map_err(LocalStoreError::Source)?;
        for mut row in batch {
            if rows >= limits.max_rows {
                partial = Some(PartialReason::RowLimit {
                    limit: limits.max_rows,
                });
                break 'batches;
            }
            let size: u64 = row.iter().map(cell_bytes).sum();
            if bytes.saturating_add(size) > limits.max_bytes {
                partial = Some(limits.byte_partial());
                break 'batches;
            }
            row.resize(columns.len(), CellValue::Null);
            hash_row(&mut hasher, &row);
            let values = row
                .into_iter()
                .zip(columns)
                .map(|(cell, column)| carrier_value(cell, column.mapped.is_blob()));
            appender.append_row(::duckdb::appender_params_from_iter(values))?;
            rows += 1;
            bytes += size;
            if rows.is_multiple_of(CANCEL_CHECK_ROWS) {
                cancel.check()?;
            }
        }
    }
    appender.flush()?;
    Ok(Appended {
        rows,
        bytes,
        partial,
        checksum: hex(&hasher.finalize()),
    })
}

/// A value in its staging form: text for VARCHAR-staged columns, bytes for
/// BLOB-staged ones.
fn carrier_value(cell: CellValue, blob: bool) -> Value {
    let text = match cell {
        CellValue::Null => return Value::Null,
        CellValue::Bytes(bytes) if blob => return Value::Blob(bytes),
        CellValue::String(s) if blob => return Value::Blob(s.into_bytes()),
        CellValue::Bytes(bytes) => blob_text(&bytes),
        CellValue::String(s) => s,
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => float_text(f),
    };
    if blob {
        Value::Blob(text.into_bytes())
    } else {
        Value::Text(text)
    }
}

/// DuckDB's text form of a blob: `\xAB` per byte.
fn blob_text(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\x{b:02X}")).collect()
}

/// Shortest round-tripping text; the non-finite values in the spelling
/// DuckDB casts back.
fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        value.to_string()
    }
}

/// Feed one row into the checksum: a tag and length-prefixed payload per
/// cell, so different rows never hash alike by concatenation.
fn hash_row(hasher: &mut Sha256, row: &[CellValue]) {
    for cell in row {
        let (tag, payload): (u8, std::borrow::Cow<'_, [u8]>) = match cell {
            CellValue::Null => (0, (&[][..]).into()),
            CellValue::Bool(b) => (1, vec![u8::from(*b)].into()),
            CellValue::I64(n) => (2, n.to_le_bytes().to_vec().into()),
            CellValue::F64(f) => (3, f.to_bits().to_le_bytes().to_vec().into()),
            CellValue::String(s) => (4, s.as_bytes().into()),
            CellValue::Bytes(b) => (5, b.as_slice().into()),
        };
        hasher.update([tag]);
        hasher.update((payload.len() as u64).to_le_bytes());
        hasher.update(&payload);
    }
    hasher.update([0xff]);
}

/// Pick each column's final type from the staged values: the mapped type
/// when every value converts, VARCHAR when one does not; undeclared
/// decimals sized to the widest value; untyped columns BIGINT, DOUBLE or
/// VARCHAR.
fn resolve_types(
    conn: &::duckdb::Connection,
    staging: &str,
    columns: &[PlannedColumn],
) -> LocalResult<Vec<LocalType>> {
    let mut expressions: Vec<String> = Vec::new();
    for column in columns {
        let c = quote_ident(&column.staging);
        match column.mapped {
            LocalType::Varchar | LocalType::Blob => {}
            LocalType::InferDecimal => {
                expressions.push(format!(
                    "count(*) FILTER (WHERE {c} IS NOT NULL AND NOT \
                     regexp_full_match(trim({c}), '[+-]?([0-9]+[.]?[0-9]*|[.][0-9]+)'))"
                ));
                expressions.push(format!(
                    "max(CASE WHEN contains({c}, '.') THEN length(split_part(trim({c}), '.', 2)) ELSE 0 END)"
                ));
                expressions.push(format!(
                    "max(length(ltrim(split_part(ltrim(trim({c}), '+-'), '.', 1), '0')))"
                ));
            }
            LocalType::Infer => {
                expressions.push(format!("count({c})"));
                expressions.push(failures(&c, LocalType::BigInt));
                expressions.push(failures(&c, LocalType::Double));
            }
            mapped => expressions.push(failures(&c, mapped)),
        }
    }
    if expressions.is_empty() {
        return Ok(columns.iter().map(|c| c.mapped).collect());
    }
    let sql = format!(
        "SELECT {} FROM temp.main.{}",
        expressions
            .iter()
            .map(|e| format!("CAST({e} AS BIGINT)"))
            .collect::<Vec<_>>()
            .join(", "),
        quote_ident(staging)
    );
    let stats: Vec<Option<i64>> = conn.query_row(&sql, [], |row| {
        (0..expressions.len())
            .map(|i| row.get::<_, Option<i64>>(i))
            .collect()
    })?;
    let mut stats = stats.into_iter();
    let mut next = || stats.next().flatten().unwrap_or(0);
    Ok(columns
        .iter()
        .map(|column| match column.mapped {
            LocalType::Varchar => LocalType::Varchar,
            LocalType::Blob => LocalType::Blob,
            LocalType::InferDecimal => {
                let bad = next();
                let scale = next();
                let digits = next().max(1);
                infer_decimal(bad, digits, scale)
            }
            LocalType::Infer => {
                let present = next();
                let bad_bigint = next();
                let bad_double = next();
                if present == 0 {
                    LocalType::Varchar
                } else if bad_bigint == 0 {
                    LocalType::BigInt
                } else if bad_double == 0 {
                    LocalType::Double
                } else {
                    LocalType::Varchar
                }
            }
            mapped => {
                if next() == 0 {
                    mapped
                } else {
                    LocalType::Varchar
                }
            }
        })
        .collect())
}

/// Values of `column` that do not convert to `target`. Integer targets
/// also refuse fractions, which DuckDB's cast would silently round.
fn failures(column: &str, target: LocalType) -> String {
    let integer = matches!(
        target,
        LocalType::TinyInt
            | LocalType::SmallInt
            | LocalType::Integer
            | LocalType::BigInt
            | LocalType::UTinyInt
            | LocalType::USmallInt
            | LocalType::UInteger
            | LocalType::UBigInt
            | LocalType::HugeInt
            | LocalType::UHugeInt
    );
    let not_whole = if integer {
        format!(" OR NOT regexp_full_match(trim({column}), '[+-]?[0-9]+')")
    } else {
        String::new()
    };
    format!(
        "count(*) FILTER (WHERE {column} IS NOT NULL AND (TRY_CAST({column} AS {}) IS NULL{not_whole}))",
        target.sql()
    )
}

/// A DECIMAL wide enough for `digits` integer digits and `scale` fraction
/// digits: DECIMAL(18, s) when it fits (stored in eight bytes), else
/// DECIMAL(38, s), else VARCHAR (exact text beats a rounded number).
fn infer_decimal(bad: i64, digits: i64, scale: i64) -> LocalType {
    if bad > 0 || scale < 0 || digits < 0 {
        return LocalType::Varchar;
    }
    let total = digits + scale;
    let precision = if total <= 18 {
        18
    } else if total <= i64::from(super::type_map::MAX_DECIMAL_PRECISION) {
        38
    } else {
        return LocalType::Varchar;
    };
    LocalType::Decimal {
        precision,
        scale: scale as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: String::new(),
            nullable: true,
        }
    }

    #[test]
    fn column_names_are_unique_and_never_blank() {
        let names = unique_column_names(&[col("id"), col(""), col("ID"), col("id_2"), col("id")]);
        assert_eq!(names, ["id", "column2", "ID_2", "id_2_2", "id_3"]);
    }

    #[test]
    fn decimal_sizing() {
        assert_eq!(
            infer_decimal(0, 3, 2),
            LocalType::Decimal {
                precision: 18,
                scale: 2
            }
        );
        assert_eq!(
            infer_decimal(0, 30, 4),
            LocalType::Decimal {
                precision: 38,
                scale: 4
            }
        );
        assert_eq!(infer_decimal(0, 30, 10), LocalType::Varchar);
        assert_eq!(infer_decimal(1, 1, 0), LocalType::Varchar);
    }

    #[test]
    fn carriers_keep_values_exact() {
        assert_eq!(
            carrier_value(CellValue::F64(0.1), false),
            Value::Text("0.1".into())
        );
        assert_eq!(
            carrier_value(CellValue::F64(f64::NEG_INFINITY), false),
            Value::Text("-Infinity".into())
        );
        assert_eq!(
            carrier_value(CellValue::I64(i64::MIN), false),
            Value::Text(i64::MIN.to_string())
        );
        assert_eq!(
            carrier_value(CellValue::Bytes(vec![0, 255]), false),
            Value::Text("\\x00\\xFF".into())
        );
        assert_eq!(
            carrier_value(CellValue::Bytes(vec![1, 2]), true),
            Value::Blob(vec![1, 2])
        );
        assert_eq!(carrier_value(CellValue::Null, true), Value::Null);
    }

    #[test]
    fn row_hash_separates_cells() {
        let digest = |row: &[CellValue]| {
            let mut hasher = Sha256::new();
            hash_row(&mut hasher, row);
            hex(&hasher.finalize())
        };
        let a = digest(&[
            CellValue::String("ab".into()),
            CellValue::String("c".into()),
        ]);
        let b = digest(&[
            CellValue::String("a".into()),
            CellValue::String("bc".into()),
        ]);
        let c = digest(&[CellValue::I64(1)]);
        let d = digest(&[CellValue::String("1".into())]);
        assert_ne!(a, b);
        assert_ne!(c, d);
    }
}
