//! Cell coercions shared by the analytics: declared-type classes, numeric
//! text (decimals arrive as strings from several engines), display text,
//! and a stable cell hash.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

/// The family a column's declared database type belongs to, across every
/// engine's spelling (`int8`, `Int64`, `bigint`, `Nullable(UInt32)`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredClass {
    Integer,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Time,
    Uuid,
    Json,
    Text,
    Binary,
    Unknown,
}

impl DeclaredClass {
    pub fn is_numeric(self) -> bool {
        matches!(self, DeclaredClass::Integer | DeclaredClass::Decimal)
    }

    pub fn is_temporal(self) -> bool {
        matches!(self, DeclaredClass::Date | DeclaredClass::DateTime)
    }
}

/// Classify a declared column type. Empty or unrecognised names are
/// [`DeclaredClass::Unknown`], which makes the analytics read the values.
pub fn declared_class(data_type: &str) -> DeclaredClass {
    let mut lower = data_type.trim().to_ascii_lowercase();
    // ClickHouse wraps the real type: Nullable(Int32), LowCardinality(String).
    loop {
        let unwrapped = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|wrapper| lower.strip_prefix(wrapper))
            .and_then(|inner| inner.strip_suffix(')'))
            .map(str::to_string);
        match unwrapped {
            Some(inner) => lower = inner,
            None => break,
        }
    }
    let base = lower
        .split(['(', '['])
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default();
    match base {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "int1" | "int2"
        | "int4" | "int8" | "int16" | "int32" | "int64" | "int128" | "int256" | "uint8"
        | "uint16" | "uint32" | "uint64" | "uint128" | "uint256" | "utinyint" | "usmallint"
        | "uinteger" | "ubigint" | "hugeint" | "uhugeint" | "serial" | "bigserial"
        | "smallserial" | "serial4" | "serial8" | "pls_integer" | "binary_integer" | "signed"
        | "unsigned" => DeclaredClass::Integer,
        "decimal" | "numeric" | "number" | "dec" | "money" | "smallmoney" | "float" | "real"
        | "double" | "float4" | "float8" | "float32" | "float64" | "binary_float"
        | "binary_double" | "decfloat" | "decimal32" | "decimal64" | "decimal128"
        | "decimal256" => DeclaredClass::Decimal,
        "bool" | "boolean" | "bit" => DeclaredClass::Boolean,
        "date" | "date32" => DeclaredClass::Date,
        "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "timestamp"
        | "timestamptz" | "datetime64" | "timestamp_s" | "timestamp_ms" | "timestamp_ns"
        | "timestamp_us" => DeclaredClass::DateTime,
        "time" | "timetz" => DeclaredClass::Time,
        "uuid" | "uniqueidentifier" => DeclaredClass::Uuid,
        "json" | "jsonb" => DeclaredClass::Json,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "tinytext"
        | "mediumtext" | "longtext" | "string" | "fixedstring" | "citext" | "clob" | "nclob"
        | "varchar2" | "nvarchar2" | "character" | "bpchar" | "name" | "enum" | "enum8"
        | "enum16" | "set" | "xml" | "sysname" => DeclaredClass::Text,
        "binary" | "varbinary" | "image" | "blob" | "tinyblob" | "mediumblob" | "longblob"
        | "bytea" | "raw" | "bytes" => DeclaredClass::Binary,
        _ => DeclaredClass::Unknown,
    }
}

/// The value of a plain decimal literal: optional sign, digits, optional
/// fraction, optional exponent. Stricter than `f64::from_str`: `inf`, `NaN`,
/// grouping separators and currency symbols are not numbers here.
pub fn numeric_literal(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix(['-', '+']).unwrap_or(trimmed);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if let Some(exp) = exponent {
        let digits = exp.strip_prefix(['-', '+']).unwrap_or(exp);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    trimmed.parse::<f64>().ok()
}

/// Whether `text` is a whole-number literal (optional sign, digits only).
pub fn is_integer_literal(text: &str) -> bool {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix(['-', '+']).unwrap_or(trimmed);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

const CURRENCY_SYMBOLS: &[&str] = &[
    "$", "€", "£", "¥", "₹", "₩", "₽", "₺", "₪", "R$", "CHF", "USD", "EUR", "GBP", "JPY",
];

/// The amount of a currency-looking text such as `$1,234.50`, `-€12`,
/// `(£3.00)` or `12.00 USD`. A currency marker is required, so a bare
/// number is not currency.
pub fn currency_value(text: &str) -> Option<f64> {
    let mut rest = text.trim();
    let mut negative = false;
    if let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        negative = true;
        rest = inner.trim();
    }
    if let Some(after) = rest.strip_prefix('-') {
        negative = !negative;
        rest = after.trim_start();
    }
    let mut found = false;
    for symbol in CURRENCY_SYMBOLS {
        if let Some(after) = rest.strip_prefix(symbol) {
            rest = after.trim_start();
            found = true;
            break;
        }
        if let Some(before) = rest.strip_suffix(symbol) {
            rest = before.trim_end();
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    if let Some(after) = rest.strip_prefix('-') {
        negative = !negative;
        rest = after;
    }
    let (int_part, frac_part) = rest.split_once('.').unwrap_or((rest, ""));
    let groups: Vec<&str> = int_part.split(',').collect();
    let grouped_ok = groups.len() == 1
        || (!groups[0].is_empty()
            && groups[0].len() <= 3
            && groups[1..].iter().all(|group| group.len() == 3));
    if int_part.is_empty()
        || !grouped_ok
        || !int_part.bytes().all(|b| b.is_ascii_digit() || b == b',')
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits: String = int_part.chars().filter(|c| *c != ',').collect();
    let value = format!(
        "{digits}.{}",
        if frac_part.is_empty() { "0" } else { frac_part }
    )
    .parse::<f64>()
    .ok()?;
    Some(if negative { -value } else { value })
}

/// The numeric value of a cell: integers and floats as-is (NaN and
/// infinities included), and numeric text. Booleans, bytes and other text
/// are not numbers.
pub fn cell_number(cell: &CellValue) -> Option<f64> {
    match cell {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(f) => Some(*f),
        CellValue::String(text) => numeric_literal(text),
        CellValue::Null | CellValue::Bool(_) | CellValue::Bytes(_) => None,
    }
}

/// [`cell_number`] restricted to finite values.
pub fn finite_number(cell: &CellValue) -> Option<f64> {
    cell_number(cell).filter(|value| value.is_finite())
}

/// Display text for a cell, `None` for NULL. Bytes render as `0x…` hex,
/// capped at 64 bytes.
pub fn cell_text(cell: &CellValue) -> Option<Cow<'_, str>> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        CellValue::I64(n) => Some(Cow::Owned(n.to_string())),
        CellValue::F64(f) => Some(Cow::Owned(format_float(*f))),
        CellValue::String(text) => Some(Cow::Borrowed(text)),
        CellValue::Bytes(bytes) => {
            let mut hex = String::with_capacity(2 + bytes.len().min(64) * 2);
            hex.push_str("0x");
            for byte in bytes.iter().take(64) {
                hex.push_str(&format!("{byte:02X}"));
            }
            if bytes.len() > 64 {
                hex.push('…');
            }
            Some(Cow::Owned(hex))
        }
    }
}

/// Shortest round-tripping text for a float, with `NaN`, `Infinity` and
/// `-Infinity` spelled out.
pub fn format_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        let text = format!("{value}");
        if text == "-0" { "0".to_string() } else { text }
    }
}

/// Truncate to at most `max_chars` characters, on a character boundary.
pub fn truncate_chars(text: &str, max_chars: usize) -> Cow<'_, str> {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => Cow::Owned(format!("{}…", &text[..at])),
        None => Cow::Borrowed(text),
    }
}

/// A 64-bit hash of a cell that is stable across runs, processes and
/// platforms (sketch state and fingerprints must not depend on the std
/// hasher's random keys). Typed: `I64(1)`, `F64(1.0)` and `"1"` differ;
/// every NaN hashes alike and `-0.0` hashes as `0.0`.
pub fn cell_hash(cell: &CellValue, seed: u64) -> u64 {
    let mut hasher = StableHasher::new(seed);
    hash_cell_into(cell, &mut hasher);
    hasher.finish()
}

/// [`cell_hash`] over a tuple of cells.
pub fn cells_hash<'a>(cells: impl IntoIterator<Item = &'a CellValue>, seed: u64) -> u64 {
    let mut hasher = StableHasher::new(seed);
    for cell in cells {
        hash_cell_into(cell, &mut hasher);
    }
    hasher.finish()
}

fn hash_cell_into(cell: &CellValue, hasher: &mut StableHasher) {
    match cell {
        CellValue::Null => hasher.write(&[0]),
        CellValue::Bool(b) => hasher.write(&[1, u8::from(*b)]),
        CellValue::I64(n) => {
            hasher.write(&[2]);
            hasher.write(&n.to_le_bytes());
        }
        CellValue::F64(f) => {
            let bits = if f.is_nan() {
                f64::NAN.to_bits()
            } else if *f == 0.0 {
                0f64.to_bits()
            } else {
                f.to_bits()
            };
            hasher.write(&[3]);
            hasher.write(&bits.to_le_bytes());
        }
        CellValue::String(text) => {
            hasher.write(&[4]);
            hasher.write(&(text.len() as u64).to_le_bytes());
            hasher.write(text.as_bytes());
        }
        CellValue::Bytes(bytes) => {
            hasher.write(&[5]);
            hasher.write(&(bytes.len() as u64).to_le_bytes());
            hasher.write(bytes);
        }
    }
}

/// FNV-1a over the bytes, finished with a splitmix64 mix so the high bits
/// (which HyperLogLog reads) are well distributed.
pub struct StableHasher {
    state: u64,
}

impl StableHasher {
    pub fn new(seed: u64) -> Self {
        Self {
            state: 0xcbf2_9ce4_8422_2325 ^ splitmix64(seed),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        splitmix64(self.state)
    }
}

pub fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::*;

    #[test]
    fn declared_types_classify_across_engines() {
        let cases = [
            ("int", DeclaredClass::Integer),
            ("BIGINT", DeclaredClass::Integer),
            ("Nullable(UInt32)", DeclaredClass::Integer),
            ("int unsigned", DeclaredClass::Integer),
            ("decimal(18,2)", DeclaredClass::Decimal),
            ("double precision", DeclaredClass::Decimal),
            ("Float64", DeclaredClass::Decimal),
            ("money", DeclaredClass::Decimal),
            ("bit", DeclaredClass::Boolean),
            ("boolean", DeclaredClass::Boolean),
            ("date", DeclaredClass::Date),
            ("timestamp with time zone", DeclaredClass::DateTime),
            ("DateTime64(3)", DeclaredClass::DateTime),
            ("datetimeoffset(7)", DeclaredClass::DateTime),
            ("time", DeclaredClass::Time),
            ("uniqueidentifier", DeclaredClass::Uuid),
            ("jsonb", DeclaredClass::Json),
            ("nvarchar(max)", DeclaredClass::Text),
            ("LowCardinality(String)", DeclaredClass::Text),
            ("character varying(20)", DeclaredClass::Text),
            ("bytea", DeclaredClass::Binary),
            ("", DeclaredClass::Unknown),
            ("geography", DeclaredClass::Unknown),
        ];
        for (input, expected) in cases {
            assert_eq!(declared_class(input), expected, "{input}");
        }
    }

    #[test]
    fn numeric_literals_are_strict() {
        let cases: &[(&str, Option<f64>)] = &[
            ("12", Some(12.0)),
            (" -3.5 ", Some(-3.5)),
            ("+4", Some(4.0)),
            (".5", Some(0.5)),
            ("1e3", Some(1000.0)),
            ("12345678901234567890.123", Some(12345678901234567890.123)),
            ("", None),
            ("-", None),
            ("1,000", None),
            ("$5", None),
            ("NaN", None),
            ("inf", None),
            ("1e", None),
            ("١٢", None),
        ];
        for (input, expected) in cases {
            assert_eq!(numeric_literal(input), *expected, "{input}");
        }
        assert!(is_integer_literal("-42"));
        assert!(!is_integer_literal("4.2"));
    }

    #[test]
    fn currency_texts_parse_with_a_marker_only() {
        let cases: &[(&str, Option<f64>)] = &[
            ("$1,234.50", Some(1234.5)),
            ("-€12", Some(-12.0)),
            ("€-12", Some(-12.0)),
            ("(£3.00)", Some(-3.0)),
            ("12.00 USD", Some(12.0)),
            ("¥1000", Some(1000.0)),
            ("1234", None),
            ("$1,23", None),
            ("$", None),
            ("$abc", None),
        ];
        for (input, expected) in cases {
            assert_eq!(currency_value(input), *expected, "{input}");
        }
    }

    #[test]
    fn cells_coerce_to_numbers_and_text() {
        assert_eq!(cell_number(&i(3)), Some(3.0));
        assert_eq!(cell_number(&s("2.50")), Some(2.5));
        assert!(cell_number(&f(f64::NAN)).unwrap().is_nan());
        assert_eq!(finite_number(&f(f64::INFINITY)), None);
        assert_eq!(cell_number(&CellValue::Bool(true)), None);
        assert_eq!(cell_text(&CellValue::Null), None);
        assert_eq!(cell_text(&f(-0.0)).unwrap(), "0");
        assert_eq!(cell_text(&f(f64::NEG_INFINITY)).unwrap(), "-Infinity");
        assert_eq!(
            cell_text(&CellValue::Bytes(vec![0xAB, 1])).unwrap(),
            "0xAB01"
        );
        assert_eq!(truncate_chars("héllo", 2), "hé…");
        assert_eq!(truncate_chars("hé", 2), "hé");
    }

    #[test]
    fn cell_hash_is_stable_and_typed() {
        assert_eq!(cell_hash(&i(1), 0), cell_hash(&i(1), 0));
        assert_ne!(cell_hash(&i(1), 0), cell_hash(&f(1.0), 0));
        assert_ne!(cell_hash(&i(1), 0), cell_hash(&s("1"), 0));
        assert_ne!(cell_hash(&i(1), 0), cell_hash(&i(1), 1));
        assert_eq!(cell_hash(&f(0.0), 0), cell_hash(&f(-0.0), 0));
        assert_eq!(cell_hash(&f(f64::NAN), 0), cell_hash(&f(-f64::NAN), 0));
        assert_ne!(
            cells_hash(&[s("ab"), s("c")], 0),
            cells_hash(&[s("a"), s("bc")], 0)
        );
        // Pinned so a change to the hash (which would silently reshuffle
        // persisted sketches) fails loudly.
        assert_eq!(cell_hash(&s("sqlly"), 0), 12_995_653_849_599_148_265);
        assert_eq!(splitmix64(0), 0xe220_a839_7b1d_cdaf);
    }
}
