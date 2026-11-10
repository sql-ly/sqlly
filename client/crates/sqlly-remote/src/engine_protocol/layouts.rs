//! Protocol-owned typed operation layouts.
//!
//! Versioned binary request/frame body shapes shared by the engine (decoder
//! side, `sqlly-cli`) and its remote clients (encoder side,
//! `sqlly-remote-services`). Before this module the two sides carried
//! hand-mirrored encode/decode pairs annotated "MUST match byte-for-byte";
//! now each layout is encoded and decoded by ONE implementation, so the pair
//! cannot drift. Dispatch, database access, and app-level types stay in the
//! apps — these layouts speak protocol-local wire structs (`*Wire`) with
//! opaque `u8` kind bytes that each side maps to its own enums.
//!
//! Every decoder validates the leading `BINARY_REQUEST_LAYOUT_VERSION` (for
//! request bodies), enforces the same minimum-footprint guards on counts the
//! engine has always used (allocation-bomb resistance), and returns typed
//! [`ProtocolError`]s — never panics — on truncated or malformed input.

use crate::engine_protocol::constants::BINARY_REQUEST_LAYOUT_VERSION;
use crate::engine_protocol::error::{ProtocolError, ProtocolResult};
use crate::engine_protocol::reader::BinaryReader;
use crate::engine_protocol::writer::BinaryWriter;

/// Start a request body: `[u16 layout_version][string id]` — the prefix every
/// binary request shares.
pub fn request_header(id: &str) -> BinaryWriter {
    BinaryWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
}

/// Read and validate the request-body prefix, returning the request id and a
/// reader positioned at the operation payload.
pub fn read_request_header(body: &[u8]) -> ProtocolResult<(String, BinaryReader<'_>)> {
    let mut r = BinaryReader::new(body);
    let version = r.read_u16_le("layout_version")?;
    if version != BINARY_REQUEST_LAYOUT_VERSION {
        return Err(ProtocolError::decode(format!(
            "unsupported binary request layout version {version} (expected {BINARY_REQUEST_LAYOUT_VERSION})"
        )));
    }
    let id = r.read_string("id")?;
    Ok((id, r))
}

// ---------------------------------------------------------------------------
// Header-only requests (ping, shutdown) and cancel
// ---------------------------------------------------------------------------

/// Encode a request that carries nothing beyond the header (ping, shutdown).
pub fn encode_header_only(id: &str) -> Vec<u8> {
    request_header(id).done()
}

/// Decode a header-only request body to its id.
pub fn decode_header_only(body: &[u8]) -> ProtocolResult<String> {
    let (id, r) = read_request_header(body)?;
    r.ensure_at_end()?;
    Ok(id)
}

/// Encode a cancel request: header + the target request id.
pub fn encode_cancel(id: &str, target_id: &str) -> Vec<u8> {
    request_header(id).string(target_id).done()
}

/// Decode a cancel request to `(id, target_id)`.
pub fn decode_cancel(body: &[u8]) -> ProtocolResult<(String, String)> {
    let (id, mut r) = read_request_header(body)?;
    let target_id = r.read_string("target_id")?;
    r.ensure_at_end()?;
    Ok((id, target_id))
}

// ---------------------------------------------------------------------------
// execute_query
// ---------------------------------------------------------------------------

/// A decoded `execute_query` request body.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteQueryWire {
    pub id: String,
    /// The connection payload as JSON (the engine deserializes it into its
    /// own connection type; the layout treats it as an opaque string).
    pub connection_json: String,
    pub sql: String,
    pub row_buffer: Option<u32>,
    /// Session/tab key for warm-connection affinity.
    pub connection_key: Option<String>,
    /// Option bits (`EXECUTE_QUERY_OPTION_*`); `0` for a body without the
    /// trailing options word (every client before
    /// `CAPABILITY_QUERY_CELL_PREVIEWS`).
    pub options: u32,
}

pub fn encode_execute_query(
    id: &str,
    connection_json: &str,
    sql: &str,
    row_buffer: Option<u32>,
    connection_key: Option<&str>,
) -> Vec<u8> {
    request_header(id)
        .string(connection_json)
        .string(sql)
        .opt_u32(row_buffer)
        .opt_string(connection_key)
        .done()
}

/// [`encode_execute_query`] plus the trailing options word. `options == 0`
/// encodes exactly the legacy body, so an engine that predates the word
/// never sees it; send non-zero options only to an engine that advertised
/// `CAPABILITY_QUERY_CELL_PREVIEWS`.
pub fn encode_execute_query_with_options(
    id: &str,
    connection_json: &str,
    sql: &str,
    row_buffer: Option<u32>,
    connection_key: Option<&str>,
    options: u32,
) -> Vec<u8> {
    let w = request_header(id)
        .string(connection_json)
        .string(sql)
        .opt_u32(row_buffer)
        .opt_string(connection_key);
    if options == 0 {
        w.done()
    } else {
        w.u32_le(options).done()
    }
}

pub fn decode_execute_query(body: &[u8]) -> ProtocolResult<ExecuteQueryWire> {
    let (id, mut r) = read_request_header(body)?;
    let connection_json = r.read_string("connection_json")?;
    let sql = r.read_string("sql")?;
    let row_buffer = r.read_optional_u32("row_buffer")?;
    let connection_key = r.read_optional_string("connection_key")?;
    // Newer clients append an options word; legacy bodies end here.
    let options = if r.remaining() == 0 {
        0
    } else {
        r.read_u32_le("options")?
    };
    r.ensure_at_end()?;
    Ok(ExecuteQueryWire {
        id,
        connection_json,
        sql,
        row_buffer,
        connection_key,
        options,
    })
}

// ---------------------------------------------------------------------------
// truncated_cells (FRAME_TAG_TRUNCATED_CELLS)
// ---------------------------------------------------------------------------

/// Kind byte of a [`TruncatedCellWire`].
pub const TRUNCATED_KIND_TEXT: u8 = 0;
/// Kind byte of a [`TruncatedCellWire`].
pub const TRUNCATED_KIND_BYTES: u8 = 1;

/// One cell cut to a preview: absolute row within its result set, column,
/// the full value's length in bytes, and whether it was text or binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncatedCellWire {
    pub row: u64,
    pub column: u32,
    pub total_len: u64,
    pub kind: u8,
}

/// A decoded `truncated_cells` frame body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedCellsWire {
    pub id: String,
    pub result_set_index: u32,
    pub cells: Vec<TruncatedCellWire>,
}

/// Bytes one encoded [`TruncatedCellWire`] occupies.
const TRUNCATED_CELL_WIRE_BYTES: usize = 8 + 4 + 8 + 1;

/// Encode a `truncated_cells` body:
/// `[string id][u32 rs_idx][u32 count]` then per cell
/// `[i64 row][u32 column][i64 total_len][u8 kind]`.
pub fn encode_truncated_cells(
    id: &str,
    result_set_index: u32,
    cells: &[TruncatedCellWire],
) -> Vec<u8> {
    let mut w = BinaryWriter::new()
        .string(id)
        .u32_le(result_set_index)
        .u32_le(cells.len() as u32);
    for cell in cells {
        w = w
            .i64_le(cell.row as i64)
            .u32_le(cell.column)
            .i64_le(cell.total_len as i64)
            .u8(cell.kind);
    }
    w.done()
}

/// Decode a `truncated_cells` body. The count is checked against the bytes
/// present before anything is allocated.
pub fn decode_truncated_cells(body: &[u8]) -> ProtocolResult<TruncatedCellsWire> {
    let mut r = BinaryReader::new(body);
    let id = r.read_string("id")?;
    let result_set_index = r.read_u32_le("result_set_index")?;
    let count = r.read_count_with_min("cell_count", TRUNCATED_CELL_WIRE_BYTES)?;
    let mut cells = Vec::with_capacity(count);
    for _ in 0..count {
        let row = r.read_i64_le("row")?;
        let column = r.read_u32_le("column")?;
        let total_len = r.read_i64_le("total_len")?;
        let kind = r.read_u8("kind")?;
        if row < 0 || total_len < 0 {
            return Err(ProtocolError::decode(
                "truncated_cells: negative row or length",
            ));
        }
        if kind != TRUNCATED_KIND_TEXT && kind != TRUNCATED_KIND_BYTES {
            return Err(ProtocolError::decode(format!(
                "truncated_cells: unknown kind {kind}"
            )));
        }
        cells.push(TruncatedCellWire {
            row: row as u64,
            column,
            total_len: total_len as u64,
            kind,
        });
    }
    r.ensure_at_end()?;
    Ok(TruncatedCellsWire {
        id,
        result_set_index,
        cells,
    })
}

// ---------------------------------------------------------------------------
// tenant_scope (trailing optional fields on validate_sql / generate_sql /
// review_sql — ADR-012 evolution; old bodies end before these fields)
// ---------------------------------------------------------------------------

/// Per-database tenant-scope policy appended after the legacy SQL-validation
/// request fields. Old clients omit this tail; the engine then leaves tenant
/// scoping off (`require_tenant_filter=false`, `fail_closed_tenant_filter=false`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TenantScopeWire {
    pub require_tenant_filter: bool,
    pub fail_closed_tenant_filter: bool,
    pub tenant_column_folds: Option<Vec<String>>,
}

/// Append tenant-scope trailing fields after the legacy validate/generate/review
/// payload (`require`, `fail_closed`, optional custom column list).
pub fn append_tenant_scope_trailing(w: BinaryWriter, scope: &TenantScopeWire) -> BinaryWriter {
    w.bool(scope.require_tenant_filter)
        .bool(scope.fail_closed_tenant_filter)
        .opt_string_list(scope.tenant_column_folds.as_deref())
}

/// Decode tenant-scope trailing fields when present. Returns `None` when the
/// reader is already at end (legacy three-field validate bodies).
pub fn decode_tenant_scope_trailing_if_present(
    r: &mut BinaryReader<'_>,
) -> ProtocolResult<Option<TenantScopeWire>> {
    if r.remaining() == 0 {
        return Ok(None);
    }
    let require_tenant_filter = r.read_bool("require_tenant_filter")?;
    let fail_closed_tenant_filter = r.read_bool("fail_closed_tenant_filter")?;
    let tenant_column_folds = r.read_opt_string_list("tenant_column_folds")?;
    Ok(Some(TenantScopeWire {
        require_tenant_filter,
        fail_closed_tenant_filter,
        tenant_column_folds,
    }))
}

// ---------------------------------------------------------------------------
// sync_catalog
// ---------------------------------------------------------------------------

/// Wire shape of one catalog object in a `sync_catalog` request. Kind fields
/// are opaque `u8`s — each side maps them to its own enums, and the engine
/// validates the values it understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogObjectWire {
    pub schema: Option<String>,
    pub name: String,
    pub kind: u8,
    pub columns: Vec<CatalogColumnWire>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<CatalogForeignKeyWire>,
    pub parameters: Vec<CatalogParameterWire>,
    pub definition: Option<String>,
    pub source_kind: u8,
    pub source_path: Option<String>,
    pub source_create_date: Option<i64>,
    pub source_modify_date: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumnWire {
    pub name: String,
    pub type_name: Option<String>,
    pub ordinal: i64,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogForeignKeyWire {
    pub columns: Vec<String>,
    pub target_schema: Option<String>,
    pub target_table: String,
    pub target_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogParameterWire {
    pub name: String,
    pub type_name: Option<String>,
    pub is_output: bool,
}

/// A decoded `sync_catalog` request body.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncCatalogWire {
    pub id: String,
    pub connection_json: String,
    pub objects: Vec<CatalogObjectWire>,
    /// Optional trailing coverage JSON (`SyncCoverage` serde). Old clients
    /// never send it (full-source semantics, the historical behavior); old
    /// engines ignore it, so senders only include it when it is not
    /// full-source — losing it must fail safe, never tombstone-by-accident.
    pub coverage_json: Option<String>,
}

// Minimum encoded footprints (bytes) per counted element, enforced on decode
// so a hostile length prefix cannot force a huge allocation. These MUST track
// the encoder: object = opt_string(4) + string(4) + kind(1) + five counts
// (4*4) + opt_string(4) + source kind(1) + opt(4) + opt(1) + opt(1) ≥ 23;
// column = 4+4+8+1 ≥ 14 (opt None is 1 byte, so 4+1+8+1=14); pk entry ≥ 4;
// fk = count(4)+opt(1)+string(4)+count(4) ≥ 13; parameter = 4+1+1 ≥ 6.
const MIN_OBJECT_BYTES: usize = 23;
const MIN_COLUMN_BYTES: usize = 14;
const MIN_STRING_BYTES: usize = 4;
const MIN_FOREIGN_KEY_BYTES: usize = 13;
const MIN_PARAMETER_BYTES: usize = 6;

pub fn encode_sync_catalog(
    id: &str,
    connection_json: &str,
    objects: &[CatalogObjectWire],
) -> Vec<u8> {
    encode_sync_catalog_with_coverage(id, connection_json, objects, None)
}

/// [`encode_sync_catalog`] with an optional trailing coverage JSON. Pass
/// `None` for full-source syncs so the bytes stay identical to what old
/// clients send.
pub fn encode_sync_catalog_with_coverage(
    id: &str,
    connection_json: &str,
    objects: &[CatalogObjectWire],
    coverage_json: Option<&str>,
) -> Vec<u8> {
    let mut w = request_header(id)
        .string(connection_json)
        .u32_le(objects.len() as u32);
    for obj in objects {
        w = encode_catalog_object(w, obj);
    }
    if let Some(coverage) = coverage_json {
        w = w.string(coverage);
    }
    w.done()
}

fn encode_catalog_object(mut w: BinaryWriter, obj: &CatalogObjectWire) -> BinaryWriter {
    w = w
        .opt_string(obj.schema.as_deref())
        .string(&obj.name)
        .u8(obj.kind);

    w = w.u32_le(obj.columns.len() as u32);
    for col in &obj.columns {
        w = w
            .string(&col.name)
            .opt_string(col.type_name.as_deref())
            .i64_le(col.ordinal)
            .bool(col.nullable);
    }

    w = w.u32_le(obj.primary_key.len() as u32);
    for col in &obj.primary_key {
        w = w.string(col);
    }

    w = w.u32_le(obj.foreign_keys.len() as u32);
    for fk in &obj.foreign_keys {
        w = w.u32_le(fk.columns.len() as u32);
        for c in &fk.columns {
            w = w.string(c);
        }
        w = w
            .opt_string(fk.target_schema.as_deref())
            .string(&fk.target_table)
            .u32_le(fk.target_columns.len() as u32);
        for c in &fk.target_columns {
            w = w.string(c);
        }
    }

    w = w.u32_le(obj.parameters.len() as u32);
    for p in &obj.parameters {
        w = w
            .string(&p.name)
            .opt_string(p.type_name.as_deref())
            .bool(p.is_output);
    }

    w.opt_string(obj.definition.as_deref())
        .u8(obj.source_kind)
        .opt_string(obj.source_path.as_deref())
        .opt_i64(obj.source_create_date)
        .opt_i64(obj.source_modify_date)
}

pub fn decode_sync_catalog(body: &[u8]) -> ProtocolResult<SyncCatalogWire> {
    let (id, mut r) = read_request_header(body)?;
    let connection_json = r.read_string("connection_json")?;

    let object_count = r.read_count_with_min("object_count", MIN_OBJECT_BYTES)?;
    let mut objects = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let p = format!("objects[{i}]");
        let schema = r.read_optional_string(&format!("{p}.schema"))?;
        let name = r.read_string(&format!("{p}.name"))?;
        let kind = r.read_u8(&format!("{p}.kind"))?;

        let column_count = r.read_count_with_min(&format!("{p}.column_count"), MIN_COLUMN_BYTES)?;
        let mut columns = Vec::with_capacity(column_count);
        for j in 0..column_count {
            let cp = format!("{p}.columns[{j}]");
            columns.push(CatalogColumnWire {
                name: r.read_string(&format!("{cp}.name"))?,
                type_name: r.read_optional_string(&format!("{cp}.type_name"))?,
                ordinal: r.read_i64_le(&format!("{cp}.ordinal"))?,
                nullable: r.read_bool(&format!("{cp}.nullable"))?,
            });
        }

        let pk_count =
            r.read_count_with_min(&format!("{p}.primary_key_count"), MIN_STRING_BYTES)?;
        let mut primary_key = Vec::with_capacity(pk_count);
        for j in 0..pk_count {
            primary_key.push(r.read_string(&format!("{p}.primary_key[{j}]"))?);
        }

        let fk_count =
            r.read_count_with_min(&format!("{p}.foreign_key_count"), MIN_FOREIGN_KEY_BYTES)?;
        let mut foreign_keys = Vec::with_capacity(fk_count);
        for j in 0..fk_count {
            let fp = format!("{p}.foreign_keys[{j}]");
            let local_col_count =
                r.read_count_with_min(&format!("{fp}.local_column_count"), MIN_STRING_BYTES)?;
            let mut columns_vec = Vec::with_capacity(local_col_count);
            for k in 0..local_col_count {
                columns_vec.push(r.read_string(&format!("{fp}.local_columns[{k}]"))?);
            }
            let target_schema = r.read_optional_string(&format!("{fp}.target_schema"))?;
            let target_table = r.read_string(&format!("{fp}.target_table"))?;
            let target_col_count =
                r.read_count_with_min(&format!("{fp}.target_column_count"), MIN_STRING_BYTES)?;
            let mut target_columns = Vec::with_capacity(target_col_count);
            for k in 0..target_col_count {
                target_columns.push(r.read_string(&format!("{fp}.target_columns[{k}]"))?);
            }
            foreign_keys.push(CatalogForeignKeyWire {
                columns: columns_vec,
                target_schema,
                target_table,
                target_columns,
            });
        }

        let parameter_count =
            r.read_count_with_min(&format!("{p}.parameter_count"), MIN_PARAMETER_BYTES)?;
        let mut parameters = Vec::with_capacity(parameter_count);
        for j in 0..parameter_count {
            let pp = format!("{p}.parameters[{j}]");
            parameters.push(CatalogParameterWire {
                name: r.read_string(&format!("{pp}.name"))?,
                type_name: r.read_optional_string(&format!("{pp}.type_name"))?,
                is_output: r.read_bool(&format!("{pp}.is_output"))?,
            });
        }

        let definition = r.read_optional_string(&format!("{p}.definition"))?;
        let source_kind = r.read_u8(&format!("{p}.source.kind"))?;
        let source_path = r.read_optional_string(&format!("{p}.source.path"))?;
        let source_create_date = r.read_optional_i64(&format!("{p}.source.create_date"))?;
        let source_modify_date = r.read_optional_i64(&format!("{p}.source.modify_date"))?;

        objects.push(CatalogObjectWire {
            schema,
            name,
            kind,
            columns,
            primary_key,
            foreign_keys,
            parameters,
            definition,
            source_kind,
            source_path,
            source_create_date,
            source_modify_date,
        });
    }

    r.ensure_at_end()?;
    let coverage_json = if r.remaining() > 0 {
        Some(r.read_string("coverage_json")?)
    } else {
        None
    };
    Ok(SyncCatalogWire {
        id,
        connection_json,
        objects,
        coverage_json,
    })
}

// ---------------------------------------------------------------------------
// rows_batch (response direction)
// ---------------------------------------------------------------------------

/// A typed cell on the wire. Mirrors the app's cell vocabulary without
/// depending on it; each side maps to/from its own cell type.
#[derive(Debug, Clone, PartialEq)]
pub enum WireCell {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
    Bytes(Vec<u8>),
}

const CELL_TAG_NULL: u8 = 0x00;
const CELL_TAG_BOOL: u8 = 0x01;
const CELL_TAG_I64: u8 = 0x02;
const CELL_TAG_F64: u8 = 0x03;
const CELL_TAG_TEXT: u8 = 0x04;
const CELL_TAG_BYTES: u8 = 0x05;

/// A decoded `rows_batch` frame body.
#[derive(Debug, Clone, PartialEq)]
pub struct RowsBatchWire {
    pub id: String,
    pub result_set_index: u32,
    pub rows: Vec<Vec<WireCell>>,
}

/// Encode a `rows_batch` body: `[string id][u32 rs_idx][u32 row_count]` then
/// per row `[u32 cell_count]` and per cell a tagged value.
pub fn encode_rows_batch(id: &str, result_set_index: u32, rows: &[Vec<WireCell>]) -> Vec<u8> {
    let mut w = BinaryWriter::new()
        .string(id)
        .u32_le(result_set_index)
        .u32_le(rows.len() as u32);
    for row in rows {
        w = w.u32_le(row.len() as u32);
        for cell in row {
            w = match cell {
                WireCell::Null => w.u8(CELL_TAG_NULL),
                WireCell::Bool(b) => w.u8(CELL_TAG_BOOL).bool(*b),
                WireCell::I64(v) => w.u8(CELL_TAG_I64).i64_le(*v),
                WireCell::F64(v) => w.u8(CELL_TAG_F64).f64_le(*v),
                WireCell::Text(s) => w.u8(CELL_TAG_TEXT).string(s),
                WireCell::Bytes(b) => w.u8(CELL_TAG_BYTES).u32_le(b.len() as u32).bytes(b),
            };
        }
    }
    w.done()
}

pub fn decode_rows_batch(body: &[u8]) -> ProtocolResult<RowsBatchWire> {
    let mut r = BinaryReader::new(body);
    let id = r.read_string("id")?;
    let result_set_index = r.read_u32_le("result_set_index")?;
    let row_count = r.read_count("row_count")?;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let cell_count = r.read_count("cell_count")?;
        let mut row = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            let tag = r.read_u8("cell_tag")?;
            let cell = match tag {
                CELL_TAG_NULL => WireCell::Null,
                CELL_TAG_BOOL => WireCell::Bool(r.read_u8("bool")? != 0),
                CELL_TAG_I64 => WireCell::I64(r.read_i64_le("i64")?),
                CELL_TAG_F64 => WireCell::F64(r.read_f64_le("f64")?),
                CELL_TAG_TEXT => WireCell::Text(r.read_string("string")?),
                CELL_TAG_BYTES => {
                    let len = r.read_u32_le("bytes_len")? as usize;
                    let mut out = Vec::with_capacity(len.min(1 << 20));
                    for _ in 0..len {
                        out.push(r.read_u8("byte")?);
                    }
                    WireCell::Bytes(out)
                }
                other => {
                    return Err(ProtocolError::decode(format!(
                        "unknown cell tag 0x{other:02x}"
                    )));
                }
            };
            row.push(cell);
        }
        rows.push(row);
    }
    Ok(RowsBatchWire {
        id,
        result_set_index,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_object() -> CatalogObjectWire {
        CatalogObjectWire {
            schema: Some("dbo".into()),
            name: "Orders".into(),
            kind: 0,
            columns: vec![CatalogColumnWire {
                name: "Id".into(),
                type_name: Some("int".into()),
                ordinal: 0,
                nullable: false,
            }],
            primary_key: vec!["Id".into()],
            foreign_keys: vec![CatalogForeignKeyWire {
                columns: vec!["CustomerId".into()],
                target_schema: Some("dbo".into()),
                target_table: "Customers".into(),
                target_columns: vec!["Id".into()],
            }],
            parameters: vec![CatalogParameterWire {
                name: "@p".into(),
                type_name: None,
                is_output: true,
            }],
            definition: None,
            source_kind: 0,
            source_path: None,
            source_create_date: Some(42),
            source_modify_date: None,
        }
    }

    #[test]
    fn sync_catalog_round_trips_byte_for_byte() {
        let body = encode_sync_catalog("req-1", "{\"server\":\"s\"}", &[sample_object()]);
        let decoded = decode_sync_catalog(&body).unwrap();
        assert_eq!(decoded.id, "req-1");
        assert_eq!(decoded.connection_json, "{\"server\":\"s\"}");
        assert_eq!(decoded.objects, vec![sample_object()]);
        // Re-encoding the decoded value reproduces the exact bytes.
        let reencoded =
            encode_sync_catalog(&decoded.id, &decoded.connection_json, &decoded.objects);
        assert_eq!(body, reencoded);
    }

    #[test]
    fn execute_query_round_trips_byte_for_byte() {
        let body = encode_execute_query("req-9", "{}", "SELECT 1", Some(64), Some("tab-1"));
        let decoded = decode_execute_query(&body).unwrap();
        assert_eq!(
            decoded,
            ExecuteQueryWire {
                id: "req-9".into(),
                connection_json: "{}".into(),
                sql: "SELECT 1".into(),
                row_buffer: Some(64),
                connection_key: Some("tab-1".into()),
                options: 0,
            }
        );
        let reencoded = encode_execute_query(
            &decoded.id,
            &decoded.connection_json,
            &decoded.sql,
            decoded.row_buffer,
            decoded.connection_key.as_deref(),
        );
        assert_eq!(body, reencoded);
    }

    #[test]
    fn rows_batch_round_trips_every_cell_kind() {
        let rows = vec![vec![
            WireCell::Null,
            WireCell::Bool(true),
            WireCell::I64(-42),
            WireCell::F64(2.5),
            WireCell::Text("héllo".into()),
            WireCell::Bytes(vec![0x00, 0xFF]),
        ]];
        let body = encode_rows_batch("req-2", 3, &rows);
        let decoded = decode_rows_batch(&body).unwrap();
        assert_eq!(decoded.id, "req-2");
        assert_eq!(decoded.result_set_index, 3);
        assert_eq!(decoded.rows, rows);
        assert_eq!(
            encode_rows_batch(&decoded.id, decoded.result_set_index, &decoded.rows),
            body
        );
    }

    #[test]
    fn header_only_and_cancel_round_trip() {
        assert_eq!(
            decode_header_only(&encode_header_only("p-1")).unwrap(),
            "p-1"
        );
        assert_eq!(
            decode_cancel(&encode_cancel("c-1", "victim")).unwrap(),
            ("c-1".to_string(), "victim".to_string())
        );
    }

    /// Options `0` is the legacy body byte for byte (an old engine, which
    /// rejects trailing bytes, never sees the word); non-zero options ride a
    /// trailing `u32` that a new engine reads back, and a legacy body still
    /// decodes with options `0`.
    #[test]
    fn execute_query_options_word_is_trailing_and_optional() {
        let legacy = encode_execute_query("req-1", "{}", "SELECT 1", Some(8), None);
        assert_eq!(
            encode_execute_query_with_options("req-1", "{}", "SELECT 1", Some(8), None, 0),
            legacy
        );
        assert_eq!(decode_execute_query(&legacy).unwrap().options, 0);
        let with = encode_execute_query_with_options(
            "req-1",
            "{}",
            "SELECT 1",
            Some(8),
            None,
            crate::engine_protocol::EXECUTE_QUERY_OPTION_CELL_PREVIEWS,
        );
        assert_eq!(with.len(), legacy.len() + 4);
        assert_eq!(&with[..legacy.len()], &legacy[..]);
        let decoded = decode_execute_query(&with).unwrap();
        assert_eq!(
            decoded.options,
            crate::engine_protocol::EXECUTE_QUERY_OPTION_CELL_PREVIEWS
        );
        assert_eq!(decoded.sql, "SELECT 1");
        // A half-written options word is an error, not a silent 0.
        assert!(decode_execute_query(&with[..with.len() - 1]).is_err());
    }

    #[test]
    fn truncated_cells_round_trip_and_reject_hostile_counts() {
        let cells = vec![
            TruncatedCellWire {
                row: 70_000,
                column: 2,
                total_len: 20 * 1024,
                kind: TRUNCATED_KIND_TEXT,
            },
            TruncatedCellWire {
                row: 3,
                column: 0,
                total_len: 5 << 30,
                kind: TRUNCATED_KIND_BYTES,
            },
        ];
        let body = encode_truncated_cells("req-4", 1, &cells);
        let decoded = decode_truncated_cells(&body).unwrap();
        assert_eq!(decoded.id, "req-4");
        assert_eq!(decoded.result_set_index, 1);
        assert_eq!(decoded.cells, cells);
        // A count far past the bytes present is refused before allocating.
        let hostile = BinaryWriter::new()
            .string("req-4")
            .u32_le(0)
            .u32_le(u32::MAX)
            .done();
        assert!(decode_truncated_cells(&hostile).is_err());
        // Unknown kind bytes are refused.
        let mut bad = encode_truncated_cells("req-4", 0, &cells[..1]);
        *bad.last_mut().unwrap() = 9;
        assert!(decode_truncated_cells(&bad).is_err());
    }

    #[test]
    fn version_mismatch_and_truncation_yield_typed_errors() {
        // Wrong layout version.
        let mut body = encode_header_only("x");
        body[0] = 0xEE;
        body[1] = 0xEE;
        assert!(decode_header_only(&body).is_err());

        // Truncated mid-field.
        let body = encode_execute_query("req", "{}", "SELECT 1", None, None);
        for cut in [1, 3, 7, body.len() - 1] {
            assert!(
                decode_execute_query(&body[..cut]).is_err(),
                "cut at {cut} must fail"
            );
        }

        // A hostile object count larger than the remaining bytes could hold.
        let mut w = request_header("evil").string("{}");
        w = w.u32_le(u32::MAX);
        assert!(decode_sync_catalog(&w.done()).is_err());
    }
}
