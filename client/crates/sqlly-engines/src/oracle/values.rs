//! Oracle column values → [`CellValue`].
//!
//! The decision of HOW to read a column is made once per column from its
//! [`OracleType`] ([`FetchKind`]); the per-row conversion then only pulls
//! the value out with the matching getter. `NUMBER` is read as Oracle's own
//! decimal text so nothing is rounded through a double: integral values
//! that fit become `I64`, everything else stays exact text. `BINARY_FLOAT`
//! and `BINARY_DOUBLE` are genuine IEEE values and map to `F64`.

use oracle::sql_type::OracleType;
use oracle::{ColumnInfo, SqlValue};
use sqlly_core::{CellValue, ResultColumn};

/// Which getter reads a column faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchKind {
    /// `NUMBER` / `FLOAT`: exact decimal text, integral → `I64`.
    Number,
    /// `BINARY_FLOAT` / `BINARY_DOUBLE`, native integers.
    Float,
    Integer,
    /// 23ai `BOOLEAN`.
    Bool,
    /// `RAW`, `LONG RAW`, `BLOB`, `BFILE`.
    Bytes,
    /// Everything else renders as text: character types, LOBs, dates,
    /// timestamps, intervals, ROWID, XML, objects.
    Text,
}

pub fn fetch_kind(oracle_type: &OracleType) -> FetchKind {
    match oracle_type {
        OracleType::Number(_, _) | OracleType::Float(_) => FetchKind::Number,
        OracleType::BinaryFloat | OracleType::BinaryDouble => FetchKind::Float,
        OracleType::Int64 | OracleType::UInt64 => FetchKind::Integer,
        OracleType::Boolean => FetchKind::Bool,
        OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB | OracleType::BFILE => {
            FetchKind::Bytes
        }
        _ => FetchKind::Text,
    }
}

/// Oracle's decimal text → the grid value: integral text that fits an
/// `i64` becomes a number, anything with a fraction, exponent, or too many
/// digits stays exact text.
pub fn number_cell(text: &str) -> CellValue {
    let trimmed = text.trim();
    match trimmed.parse::<i64>() {
        Ok(n) => CellValue::I64(n),
        Err(_) => CellValue::String(trimmed.to_string()),
    }
}

/// The result column for an Oracle column: Oracle's own type spelling,
/// lower-cased to match the other drivers (`number(10,2)`, `varchar2(50)`,
/// `timestamp(6) with time zone`).
pub fn result_column(info: &ColumnInfo) -> ResultColumn {
    ResultColumn {
        name: info.name().to_string(),
        data_type: info.oracle_type().to_string().to_ascii_lowercase(),
        nullable: info.nullable(),
    }
}

/// Convert one fetched value with the column's precomputed [`FetchKind`].
/// A value the client cannot convert (an unsupported object type, a
/// corrupt LOB) renders as `<error text>` rather than aborting the row.
pub fn cell_value(value: &SqlValue, kind: FetchKind) -> CellValue {
    if value.is_null().unwrap_or(true) {
        return CellValue::Null;
    }
    let converted = match kind {
        FetchKind::Number => value.get::<String>().map(|s| number_cell(&s)),
        FetchKind::Float => value.get::<f64>().map(CellValue::F64),
        FetchKind::Integer => value.get::<i64>().map(CellValue::I64),
        FetchKind::Bool => value.get::<bool>().map(CellValue::Bool),
        FetchKind::Bytes => value.get::<Vec<u8>>().map(CellValue::Bytes),
        FetchKind::Text => value.get::<String>().map(CellValue::String),
    };
    match converted {
        Ok(cell) => cell,
        // Fall back to the client's own text rendering before giving up.
        Err(_) => match value.get::<String>() {
            Ok(text) => CellValue::String(text),
            Err(error) => CellValue::String(format!("<{error}>")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_read_as_exact_text_and_integers_become_i64() {
        assert_eq!(fetch_kind(&OracleType::Number(10, 0)), FetchKind::Number);
        assert_eq!(fetch_kind(&OracleType::Number(0, -127)), FetchKind::Number);
        assert_eq!(fetch_kind(&OracleType::Float(126)), FetchKind::Number);
        assert_eq!(number_cell("42"), CellValue::I64(42));
        assert_eq!(number_cell(" -7 "), CellValue::I64(-7));
        assert_eq!(number_cell("1.50"), CellValue::String("1.50".into()));
        assert_eq!(number_cell("1E+30"), CellValue::String("1E+30".into()));
        assert_eq!(
            number_cell("12345678901234567890"),
            CellValue::String("12345678901234567890".into())
        );
        assert_eq!(
            number_cell("-0.000001"),
            CellValue::String("-0.000001".into())
        );
    }

    #[test]
    fn ieee_and_native_types_take_their_own_getters() {
        assert_eq!(fetch_kind(&OracleType::BinaryFloat), FetchKind::Float);
        assert_eq!(fetch_kind(&OracleType::BinaryDouble), FetchKind::Float);
        assert_eq!(fetch_kind(&OracleType::Int64), FetchKind::Integer);
        assert_eq!(fetch_kind(&OracleType::Boolean), FetchKind::Bool);
    }

    #[test]
    fn binary_types_become_bytes() {
        for t in [
            OracleType::Raw(16),
            OracleType::LongRaw,
            OracleType::BLOB,
            OracleType::BFILE,
        ] {
            assert_eq!(fetch_kind(&t), FetchKind::Bytes, "{t}");
        }
    }

    #[test]
    fn character_temporal_and_document_types_render_as_text() {
        for t in [
            OracleType::Varchar2(50),
            OracleType::NVarchar2(50),
            OracleType::Char(3),
            OracleType::NChar(3),
            OracleType::CLOB,
            OracleType::NCLOB,
            OracleType::Long,
            OracleType::Date,
            OracleType::Timestamp(6),
            OracleType::TimestampTZ(6),
            OracleType::TimestampLTZ(6),
            OracleType::IntervalDS(2, 6),
            OracleType::IntervalYM(2),
            OracleType::Rowid,
            OracleType::Xml,
            OracleType::Json,
        ] {
            assert_eq!(fetch_kind(&t), FetchKind::Text, "{t}");
        }
    }
}
