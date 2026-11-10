//! Test Data Generator — deterministic, dialect-correct INSERT scripts for a
//! table, from its real column metadata.
//!
//! Script-first: the dialog loads columns (read-only), generates values
//! locally, and opens the INSERT script in a query tab. Nothing is written
//! to the server from here. All value generation is pure and seeded, so the
//! same inputs always script the same data.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_core::CellValue;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

/// Hard cap on scripted rows; the UI clamps to this.
const MAX_ROWS: usize = 10_000;
/// Rows per multi-row INSERT statement. SQL Server caps a VALUES list at
/// 1000 rows; 500 stays comfortably under every engine's limits.
const ROWS_PER_STATEMENT: usize = 500;

/// One target column, as loaded from the engine's catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    /// Identity / auto-increment / computed — the engine fills these itself,
    /// so the script must not.
    pub skip: bool,
    /// The only values the column accepts (ClickHouse `Enum8('a' = 1, …)`
    /// members, IP addresses): the generator picks one instead of inventing
    /// text the engine would reject.
    pub choices: Vec<String>,
    /// A literal the column always receives (`[]` for ClickHouse arrays,
    /// `map()` for maps): collection types have no scalar generator.
    pub fixed_literal: Option<String>,
}

/// The value families the generator can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueGenerator {
    Name,
    Word,
    Text,
    Email,
    Integer,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Time,
    Guid,
    Json,
    Binary,
}

impl ValueGenerator {
    pub fn all() -> [ValueGenerator; 13] {
        [
            Self::Name,
            Self::Word,
            Self::Text,
            Self::Email,
            Self::Integer,
            Self::Decimal,
            Self::Boolean,
            Self::Date,
            Self::DateTime,
            Self::Time,
            Self::Guid,
            Self::Json,
            Self::Binary,
        ]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Name => "explorer.test_data.generator_name",
            Self::Word => "explorer.test_data.generator_word",
            Self::Text => "explorer.test_data.generator_text",
            Self::Email => "explorer.test_data.generator_email",
            Self::Integer => "explorer.test_data.generator_integer",
            Self::Decimal => "explorer.test_data.generator_decimal",
            Self::Boolean => "explorer.test_data.generator_boolean",
            Self::Date => "explorer.test_data.generator_date",
            Self::DateTime => "explorer.test_data.generator_datetime",
            Self::Time => "explorer.test_data.generator_time",
            Self::Guid => "explorer.test_data.generator_guid",
            Self::Json => "explorer.test_data.generator_json",
            Self::Binary => "explorer.test_data.generator_binary",
        })
    }

    pub fn next(self) -> Self {
        let all = Self::all();
        let index = all.iter().position(|g| *g == self).unwrap_or(0);
        all[(index + 1) % all.len()]
    }
}

/// Lowercase base type name (last segment if qualified) and optional `(…)` args.
fn parse_column_type(data_type: &str) -> (String, Option<String>) {
    let mut ty = data_type.trim().to_ascii_lowercase();
    // ClickHouse wraps types — Nullable(T), LowCardinality(T) — and the
    // inner type is what decides the generator.
    loop {
        let inner = ["nullable(", "lowcardinality("].iter().find_map(|wrapper| {
            ty.strip_prefix(wrapper)
                .and_then(|rest| rest.strip_suffix(')'))
                .map(|inner| inner.trim().to_string())
        });
        match inner {
            Some(inner) => ty = inner,
            None => break,
        }
    }
    let (head, args) = if let Some(open) = ty.find('(') {
        if let Some(close) = ty.rfind(')') {
            if close > open {
                (ty[..open].trim(), Some(ty[open..=close].to_string()))
            } else {
                (ty.as_str(), None)
            }
        } else {
            (ty.as_str(), None)
        }
    } else {
        (ty.as_str(), None)
    };
    let base = head.rsplit('.').next().unwrap_or(head).trim().to_string();
    (base, args)
}

fn is_guid_type(base: &str) -> bool {
    matches!(base, "uniqueidentifier" | "uuid")
}

/// `(p[,s])` of an Oracle `NUMBER(p[,s])` spelling, as parsed from the
/// catalog: `(10)` → `(10, 0)`, `(10,2)` → `(10, 2)`; `None` for bare
/// `NUMBER` (no precision, arbitrary scale).
fn oracle_number_args(args: Option<&str>) -> Option<(u32, u32)> {
    let inner = args?.trim_start_matches('(').trim_end_matches(')');
    let mut parts = inner.split(',');
    let precision = parts.next()?.trim().parse::<u32>().ok()?;
    let scale = parts
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    Some((precision, scale))
}

fn is_boolean_type(base: &str, args: Option<&str>) -> bool {
    match base {
        "bool" | "boolean" => true,
        // SQL Server `bit` or one-bit `bit(1)` — not `bit(8)` or `bit varying`.
        "bit" => args.is_none() || args == Some("(1)"),
        // MySQL boolean alias.
        "tinyint" => args == Some("(1)"),
        // Oracle has no boolean column type before 23ai; `NUMBER(1)` is its
        // convention (and what SQLLY itself maps booleans to).
        "number" => matches!(oracle_number_args(args), Some((1, 0))),
        _ => false,
    }
}

fn is_integer_type(base: &str, args: Option<&str>) -> bool {
    if matches!(base, "interval" | "point") {
        return false;
    }
    if base == "tinyint" && args == Some("(1)") {
        return false;
    }
    // Oracle `NUMBER(p)` / `NUMBER(p,0)` is an integer column (`NUMBER(1)`
    // is the boolean convention, handled first by the caller); bare
    // `NUMBER` and any scale > 0 are decimals.
    if base == "number" {
        return matches!(oracle_number_args(args), Some((p, 0)) if p > 1);
    }
    matches!(
        base,
        "int"
            | "integer"
            | "smallint"
            | "bigint"
            | "tinyint"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "int2"
            | "int4"
            | "int8"
            // ClickHouse fixed-width integers.
            | "int16"
            | "int32"
            | "int64"
            | "int128"
            | "int256"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uint128"
            | "uint256"
    )
}

/// The largest value a small integer column can hold, so generated numbers
/// never overflow it (`UInt8`, `tinyint`, `Int16`, …). `None` leaves the
/// generator's default range. PostgreSQL's `int8` is a bigint; ClickHouse's
/// is a byte, so the engine decides that one.
fn small_int_max(base: &str, db: Option<DatabaseType>) -> Option<u64> {
    match base {
        "uint8" => Some(255),
        "tinyint" => Some(127),
        "int8" if db == Some(DatabaseType::ClickHouse) => Some(127),
        "uint16" => Some(65_535),
        "int16" | "smallint" | "int2" => Some(32_767),
        _ => None,
    }
}

fn is_datetime_type(base: &str) -> bool {
    base.starts_with("datetime") || base.starts_with("timestamp")
}

fn is_date_type(base: &str) -> bool {
    base == "date" || base == "date32"
}

fn is_time_type(base: &str) -> bool {
    base == "time" || base.starts_with("time ") || base == "timetz"
}

fn is_json_type(base: &str) -> bool {
    matches!(base, "json" | "jsonb")
}

fn is_binary_type(base: &str) -> bool {
    matches!(
        base,
        "binary"
            | "varbinary"
            | "image"
            | "blob"
            | "tinyblob"
            | "mediumblob"
            | "longblob"
            | "bytea"
            | "raw"
            | "long raw"
            | "bytes"
    )
}

fn is_decimal_type(base: &str) -> bool {
    matches!(
        base,
        "decimal"
            | "numeric"
            | "money"
            | "smallmoney"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "real"
            | "double precision"
            // ClickHouse floats and fixed-scale decimals.
            | "float32"
            | "float64"
            | "decimal32"
            | "decimal64"
            | "decimal128"
            | "decimal256"
            // Oracle: bare `NUMBER` and `NUMBER(p,s>0)` (the integer and
            // boolean spellings are claimed earlier), plus its IEEE floats.
            | "number"
            | "binary_float"
            | "binary_double"
    )
}

/// String types whose declared `(n)` bounds the character length.
fn is_length_bounded_string(base: &str) -> bool {
    matches!(
        base,
        "char"
            | "nchar"
            | "varchar"
            | "nvarchar"
            | "varchar2"
            | "nvarchar2"
            | "character"
            | "character varying"
            | "string"
            | "fixedstring"
    )
}

/// ClickHouse `Enum8('sad' = 1, 'ok' = 2)` / `Enum16(…)`: the member names,
/// in declaration order. Empty for anything else.
fn enum_members(data_type: &str) -> Vec<String> {
    let (base, args) = parse_column_type(data_type);
    if !matches!(base.as_str(), "enum8" | "enum16" | "enum") {
        return Vec::new();
    }
    let Some(args) = args else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\'' {
            continue;
        }
        let mut name = String::new();
        loop {
            match chars.next() {
                Some('\\') => {
                    if let Some(escaped) = chars.next() {
                        name.push(escaped);
                    }
                }
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                    name.push('\'');
                }
                Some('\'') | None => break,
                Some(other) => name.push(other),
            }
        }
        members.push(name);
    }
    members
}

/// The value pool and fixed literal for column types no scalar generator
/// fits: enum members, IP addresses, and empty collections.
fn column_extras(data_type: &str) -> (Vec<String>, Option<String>) {
    let members = enum_members(data_type);
    if !members.is_empty() {
        return (members, None);
    }
    let (base, _) = parse_column_type(data_type);
    match base.as_str() {
        "ipv4" => (
            vec![
                "10.0.0.1".into(),
                "192.168.1.10".into(),
                "172.16.0.5".into(),
            ],
            None,
        ),
        "ipv6" => (
            vec!["::1".into(), "fe80::1".into(), "2001:db8::10".into()],
            None,
        ),
        "array" => (Vec::new(), Some("[]".into())),
        "map" => (Vec::new(), Some("map()".into())),
        _ => (Vec::new(), None),
    }
}

/// Fixed-point types whose declared `(p[,s])` bounds precision and scale.
/// `float`/`money`/`real` carry no `(p,s)` in the decimal sense, so they are
/// excluded — their arguments (if any) are bit-precision, not digit counts.
fn is_precision_scaled(base: &str) -> bool {
    matches!(base, "decimal" | "numeric" | "dec" | "number")
}

/// Length / precision limits parsed from a column's declared type. Used to keep
/// generated values inside the column's declared bounds, so the scripted INSERT
/// does not fail with truncation ("String or binary data would be truncated")
/// or numeric-overflow errors when the user runs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeLimits {
    /// Max characters for length-bounded string types (`varchar(n)` → `Some(n)`).
    /// `None` for unbounded (`varchar(max)`, `text`) or unknown.
    pub max_chars: Option<usize>,
    /// `(precision, scale)` for `decimal`/`numeric`; the integer part is limited
    /// to `precision − scale` digits and the fraction to `scale` digits.
    pub decimal: Option<(u32, u32)>,
    /// Max bytes for length-bounded binary types (`varbinary(n)` → `Some(n)`).
    /// `None` for unbounded (`varbinary(max)`, `blob`) or unknown.
    pub max_bytes: Option<usize>,
    /// Largest integer a small integer column holds (`UInt8` → 255).
    pub max_int: Option<u64>,
}

/// Parse the length/precision limits out of a declared column type.
pub fn type_limits(data_type: &str) -> TypeLimits {
    type_limits_for(None, data_type)
}

/// [`type_limits`] with the engine known, for the type spellings whose
/// width depends on it (`int8`).
pub fn type_limits_for(db: Option<DatabaseType>, data_type: &str) -> TypeLimits {
    let (base, args) = parse_column_type(data_type);
    let mut limits = TypeLimits {
        max_int: small_int_max(&base, db),
        ..TypeLimits::default()
    };
    let Some(args) = args.as_deref() else {
        return limits;
    };
    let inner = args.trim_start_matches('(').trim_end_matches(')');

    if is_length_bounded_string(&base) {
        let first = inner.split(',').next().unwrap_or("").trim();
        // `max` (SQL Server) / `-1` denote the unbounded large-object form.
        if !first.eq_ignore_ascii_case("max")
            && first != "-1"
            && let Ok(n) = first.parse::<usize>()
            && n > 0
        {
            limits.max_chars = Some(n);
        }
    }

    if is_binary_type(&base) {
        let first = inner.split(',').next().unwrap_or("").trim();
        if !first.eq_ignore_ascii_case("max")
            && first != "-1"
            && let Ok(n) = first.parse::<usize>()
            && n > 0
        {
            limits.max_bytes = Some(n);
        }
    }

    if is_precision_scaled(&base) {
        let mut parts = inner.split(',');
        let precision = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
        let scale = parts
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if let Some(precision) = precision
            && precision > 0
        {
            limits.decimal = Some((precision, scale.min(precision)));
        }
    }

    limits
}

/// Truncate `s` to at most `max` characters (char-boundary safe). No-op when
/// the limit is absent or already satisfied.
fn clamp_chars(s: String, max: Option<usize>) -> String {
    match max {
        Some(max) if s.chars().count() > max => s.chars().take(max).collect(),
        _ => s,
    }
}

/// The generator a column starts with, from its declared type and — for text
/// columns — its name (an `email` column gets emails, a `…name` column gets
/// names).
pub fn infer_generator(column_name: &str, data_type: &str) -> ValueGenerator {
    let (base, args) = parse_column_type(data_type);
    let args = args.as_deref();
    let name = column_name.to_ascii_lowercase();

    if is_guid_type(&base) {
        return ValueGenerator::Guid;
    }
    if is_json_type(&base) {
        return ValueGenerator::Json;
    }
    if is_binary_type(&base) {
        return ValueGenerator::Binary;
    }
    if is_boolean_type(&base, args) {
        return ValueGenerator::Boolean;
    }
    if is_datetime_type(&base) {
        return ValueGenerator::DateTime;
    }
    if is_date_type(&base) {
        return ValueGenerator::Date;
    }
    if is_time_type(&base) {
        return ValueGenerator::Time;
    }
    if is_integer_type(&base, args) {
        return ValueGenerator::Integer;
    }
    if is_decimal_type(&base) {
        return ValueGenerator::Decimal;
    }
    if name.contains("email") {
        return ValueGenerator::Email;
    }
    if name.contains("name") {
        return ValueGenerator::Name;
    }
    if name.contains("description") || name.contains("comment") || name.contains("note") {
        return ValueGenerator::Text;
    }
    ValueGenerator::Word
}

const FIRST_NAMES: [&str; 16] = [
    "Alex", "Sam", "Jordan", "Casey", "Riley", "Morgan", "Avery", "Quinn", "Maria", "Noah", "Ida",
    "Omar", "Lena", "Hugo", "Petra", "Dana",
];
const LAST_NAMES: [&str; 16] = [
    "Smith", "Garcia", "Chen", "Patel", "Larsen", "Kowalski", "Okafor", "Tanaka", "Novak",
    "Fischer", "Silva", "Haddad", "Berg", "Rossi", "Dubois", "Moreno",
];
const WORDS: [&str; 20] = [
    "amber", "basalt", "cedar", "delta", "ember", "fjord", "granite", "harbor", "indigo",
    "juniper", "krill", "lumen", "meadow", "nickel", "onyx", "prairie", "quartz", "reef", "summit",
    "tundra",
];

/// Small deterministic PRNG (splitmix64) — no dependency, stable across
/// platforms, good enough for sample data.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

pub(crate) fn generated_value(
    db: DatabaseType,
    generator: ValueGenerator,
    limits: TypeLimits,
    rng: &mut Rng,
) -> String {
    // Clamp string content to the column's declared length before quoting.
    let string_literal =
        |content: String| db.string_literal(&clamp_chars(content, limits.max_chars));
    match generator {
        ValueGenerator::Name => {
            let first = FIRST_NAMES[rng.below(FIRST_NAMES.len() as u64) as usize];
            let last = LAST_NAMES[rng.below(LAST_NAMES.len() as u64) as usize];
            string_literal(format!("{first} {last}"))
        }
        ValueGenerator::Word => {
            string_literal(WORDS[rng.below(WORDS.len() as u64) as usize].to_string())
        }
        ValueGenerator::Text => {
            let words: Vec<&str> = (0..3)
                .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
                .collect();
            string_literal(words.join(" "))
        }
        ValueGenerator::Email => {
            let first = FIRST_NAMES[rng.below(FIRST_NAMES.len() as u64) as usize].to_lowercase();
            let last = LAST_NAMES[rng.below(LAST_NAMES.len() as u64) as usize].to_lowercase();
            let n = rng.below(1000);
            string_literal(format!("{first}.{last}{n}@example.com"))
        }
        ValueGenerator::Integer => integer_value(limits, rng).to_string(),
        ValueGenerator::Decimal => decimal_value(limits.decimal, rng),
        ValueGenerator::Boolean => db.bool_literal(rng.below(2) == 1).to_string(),
        // The remaining string-shaped generators also route through the
        // clamping literal: their natural columns carry no character limit
        // (max_chars is None → no-op), but the user can cycle any generator
        // onto e.g. a varchar(8) column, and the value must still fit it.
        // Oracle gets typed `DATE '…'` / `TIMESTAMP '…'` literals (its
        // implicit string conversion depends on session NLS settings), except
        // when the target is a character column, where the plain string is
        // what fits.
        ValueGenerator::Date => {
            let (y, m, d) = pseudo_date(rng);
            let text = format!("{y:04}-{m:02}-{d:02}");
            if db == DatabaseType::Oracle && limits.max_chars.is_none() {
                format!("DATE '{text}'")
            } else {
                string_literal(text)
            }
        }
        ValueGenerator::DateTime => {
            let (y, m, d) = pseudo_date(rng);
            let (h, mi, s) = pseudo_time(rng);
            let text = format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}");
            if db == DatabaseType::Oracle && limits.max_chars.is_none() {
                format!("TIMESTAMP '{text}'")
            } else {
                string_literal(text)
            }
        }
        ValueGenerator::Time => {
            let (h, mi, s) = pseudo_time(rng);
            string_literal(format!("{h:02}:{mi:02}:{s:02}"))
        }
        ValueGenerator::Json => {
            // A small, valid object with a numeric and a text field — enough to
            // exercise a JSON/JSONB column. Rendered as a string literal, which
            // every engine accepts for its JSON column on INSERT.
            let word = WORDS[rng.below(WORDS.len() as u64) as usize];
            let id = rng.below(100_000);
            string_literal(format!("{{\"id\":{id},\"tag\":\"{word}\"}}"))
        }
        ValueGenerator::Binary => {
            // A handful of deterministic bytes, rendered as the engine's binary
            // literal (0x… / X'…' / '\x…') and capped to the column's declared
            // byte length so binary(2)/varbinary(4) inserts don't truncate-fail.
            let len = 4 + rng.below(5) as usize;
            let len = match limits.max_bytes {
                Some(max) => len.min(max),
                None => len,
            };
            let bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            db.bytes_literal(&bytes)
        }
        ValueGenerator::Guid => {
            let a = rng.next_u64();
            let b = rng.next_u64();
            string_literal(format!(
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                (a >> 32) as u32,
                (a >> 16) as u16,
                a as u16,
                (b >> 48) as u16,
                b & 0xFFFF_FFFF_FFFF
            ))
        }
    }
}

/// Typed cell for batch insert. SQL output of [`generated_value`] is unchanged.
pub(crate) fn generated_cell(
    _db: DatabaseType,
    generator: ValueGenerator,
    limits: TypeLimits,
    rng: &mut Rng,
) -> CellValue {
    match generator {
        ValueGenerator::Integer => CellValue::I64(integer_value(limits, rng)),
        ValueGenerator::Decimal => {
            let text = decimal_value(limits.decimal, rng);
            text.parse::<f64>()
                .map(CellValue::F64)
                .unwrap_or_else(|_| CellValue::String(text))
        }
        ValueGenerator::Boolean => CellValue::Bool(rng.below(2) == 1),
        ValueGenerator::Binary => {
            let len = 4 + rng.below(5) as usize;
            let len = match limits.max_bytes {
                Some(max) => len.min(max),
                None => len,
            };
            let bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            CellValue::Bytes(bytes)
        }
        ValueGenerator::Name => {
            let first = FIRST_NAMES[rng.below(FIRST_NAMES.len() as u64) as usize];
            let last = LAST_NAMES[rng.below(LAST_NAMES.len() as u64) as usize];
            CellValue::String(clamp_chars(format!("{first} {last}"), limits.max_chars))
        }
        ValueGenerator::Word => CellValue::String(clamp_chars(
            WORDS[rng.below(WORDS.len() as u64) as usize].to_string(),
            limits.max_chars,
        )),
        ValueGenerator::Text => {
            let words: Vec<&str> = (0..3)
                .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
                .collect();
            CellValue::String(clamp_chars(words.join(" "), limits.max_chars))
        }
        ValueGenerator::Email => {
            let first = FIRST_NAMES[rng.below(FIRST_NAMES.len() as u64) as usize].to_lowercase();
            let last = LAST_NAMES[rng.below(LAST_NAMES.len() as u64) as usize].to_lowercase();
            let n = rng.below(1000);
            CellValue::String(clamp_chars(
                format!("{first}.{last}{n}@example.com"),
                limits.max_chars,
            ))
        }
        ValueGenerator::Date => {
            let (y, m, d) = pseudo_date(rng);
            CellValue::String(clamp_chars(
                format!("{y:04}-{m:02}-{d:02}"),
                limits.max_chars,
            ))
        }
        ValueGenerator::DateTime => {
            let (y, m, d) = pseudo_date(rng);
            let (h, mi, s) = pseudo_time(rng);
            CellValue::String(clamp_chars(
                format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
                limits.max_chars,
            ))
        }
        ValueGenerator::Time => {
            let (h, mi, s) = pseudo_time(rng);
            CellValue::String(clamp_chars(
                format!("{h:02}:{mi:02}:{s:02}"),
                limits.max_chars,
            ))
        }
        ValueGenerator::Json => {
            let word = WORDS[rng.below(WORDS.len() as u64) as usize];
            let id = rng.below(100_000);
            CellValue::String(clamp_chars(
                format!("{{\"id\":{id},\"tag\":\"{word}\"}}"),
                limits.max_chars,
            ))
        }
        ValueGenerator::Guid => {
            let a = rng.next_u64();
            let b = rng.next_u64();
            CellValue::String(clamp_chars(
                format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    (a >> 32) as u32,
                    (a >> 16) as u16,
                    a as u16,
                    (b >> 48) as u16,
                    b & 0xFFFF_FFFF_FFFF
                ),
                limits.max_chars,
            ))
        }
    }
}

/// An integer in `0..100_000`, narrowed to what a declared precision can
/// hold when the column carries one (`NUMBER(3)` on Oracle, or a user who
/// cycled the Integer generator onto a `decimal(4,0)` column): the value
/// uses at most `precision − scale` digits.
fn integer_value(limits: TypeLimits, rng: &mut Rng) -> i64 {
    let mut bound = match limits.decimal {
        Some((precision, scale)) if precision.saturating_sub(scale) < 5 => {
            10u64.pow(precision.saturating_sub(scale))
        }
        _ => 100_000,
    };
    if let Some(max) = limits.max_int {
        bound = bound.min(max + 1);
    }
    rng.below(bound) as i64
}

/// A fixed-point value that fits a `decimal(p,s)` column: the integer part uses
/// at most `p − s` digits and the fraction exactly `s` digits. Without declared
/// precision (e.g. `money`, `float`, or a bare `decimal`), falls back to a
/// small default with two fractional digits.
fn decimal_value(decimal: Option<(u32, u32)>, rng: &mut Rng) -> String {
    let Some((precision, scale)) = decimal else {
        return format!("{}.{:02}", rng.below(10_000), rng.below(100));
    };
    // Cap digit counts so 10^digits stays within u64.
    let int_digits = precision.saturating_sub(scale).min(18);
    let int_part = if int_digits == 0 {
        0
    } else {
        rng.below(10u64.pow(int_digits))
    };
    if scale == 0 {
        return int_part.to_string();
    }
    let frac_digits = scale.min(18);
    let frac = rng.below(10u64.pow(frac_digits));
    format!("{int_part}.{frac:0width$}", width = frac_digits as usize)
}

/// A plausible date in 2023–2025 without pulling in a calendar crate: months
/// are capped at 28 days so every generated date exists.
fn pseudo_date(rng: &mut Rng) -> (u64, u64, u64) {
    (2023 + rng.below(3), 1 + rng.below(12), 1 + rng.below(28))
}

fn pseudo_time(rng: &mut Rng) -> (u64, u64, u64) {
    (rng.below(24), rng.below(60), rng.below(60))
}

/// The full INSERT script: seeded, dialect-correct, batched into multi-row
/// VALUES statements, with identity/computed columns skipped and nullable
/// columns receiving NULL at the requested rate.
pub fn insert_script(
    db: DatabaseType,
    schema: &str,
    table: &str,
    columns: &[(ColumnSpec, ValueGenerator)],
    row_count: usize,
    null_percent: u8,
    seed: u64,
) -> Result<String, String> {
    let active: Vec<(&ColumnSpec, ValueGenerator, TypeLimits)> = columns
        .iter()
        .filter(|(spec, _)| !spec.skip)
        .map(|(spec, generator)| (spec, *generator, type_limits_for(Some(db), &spec.data_type)))
        .collect();
    if active.is_empty() {
        return Err(crate::localization::text(
            "explorer.test_data.no_insertable_columns",
        ));
    }
    let row_count = row_count.clamp(1, MAX_ROWS);
    let null_percent = null_percent.min(100) as u64;
    let mut rng = Rng::new(seed);

    let qualified = if schema.trim().is_empty() {
        db.quote_identifier(table)
    } else {
        format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(table)
        )
    };
    let column_list = active
        .iter()
        .map(|(spec, _, _)| db.quote_identifier(&spec.name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut script = format!(
        "-- Test data generated by SQLLY: {row_count} rows, seed {seed}. Review before running.\n-- The same table, generators, and seed always produce identical data.\n"
    );
    // Oracle has no multi-row VALUES list, and `INSERT ALL` evaluates an
    // identity or sequence default once for the whole statement (every row
    // would get the same key), so each batch is one single-table
    // `INSERT … SELECT … FROM DUAL UNION ALL SELECT … FROM DUAL`.
    let oracle = db == DatabaseType::Oracle;
    let mut written = 0;
    while written < row_count {
        let batch = (row_count - written).min(ROWS_PER_STATEMENT);
        if oracle {
            script.push_str(&format!("INSERT INTO {qualified} ({column_list})\n"));
        } else {
            script.push_str(&format!("INSERT INTO {qualified} ({column_list}) VALUES\n"));
        }
        for row in 0..batch {
            let values = active
                .iter()
                .map(|(spec, generator, limits)| {
                    if spec.nullable && rng.below(100) < null_percent {
                        "NULL".to_string()
                    } else if let Some(literal) = &spec.fixed_literal {
                        literal.clone()
                    } else if !spec.choices.is_empty() {
                        let pick = rng.below(spec.choices.len() as u64) as usize;
                        db.string_literal(&spec.choices[pick])
                    } else {
                        generated_value(db, *generator, *limits, &mut rng)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            if oracle {
                let (prefix, terminator) = match (row, row + 1 == batch) {
                    (0, true) => ("  SELECT ", " FROM DUAL;\n"),
                    (0, false) => ("  SELECT ", " FROM DUAL\n"),
                    (_, true) => ("  UNION ALL SELECT ", " FROM DUAL;\n"),
                    (_, false) => ("  UNION ALL SELECT ", " FROM DUAL\n"),
                };
                script.push_str(&format!("{prefix}{values}{terminator}"));
            } else {
                let terminator = if row + 1 == batch { ";\n" } else { ",\n" };
                script.push_str(&format!("  ({values}){terminator}"));
            }
        }
        written += batch;
    }
    Ok(script)
}

/// The engine-native column-metadata query for a table. Table and schema go
/// in as escaped string literals — never interpolated raw.
pub fn columns_query(db: DatabaseType, schema: &str, table: &str) -> Option<String> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let schema = schema.trim();
    let table = table.trim();
    match db.connection_kind() {
        ConnectionKind::Tds => {
            let schema = if schema.is_empty() { "dbo" } else { schema };
            let qualified = db.string_literal(&format!("{schema}.{table}"));
            Some(format!(
                "SELECT c.name AS column_name, t.name AS data_type, c.is_nullable, \
                 CAST(CASE WHEN c.is_identity = 1 OR c.is_computed = 1 THEN 1 ELSE 0 END AS BIT) AS is_skipped \
                 FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
                 WHERE c.object_id = OBJECT_ID({qualified}) ORDER BY c.column_id"
            ))
        }
        ConnectionKind::PostgresWire => {
            let schema = if schema.is_empty() { "public" } else { schema };
            Some(format!(
                "SELECT column_name, data_type, (is_nullable = 'YES') AS is_nullable, \
                 (is_identity = 'YES' OR is_generated <> 'NEVER' OR COALESCE(column_default, '') LIKE 'nextval(%') AS is_skipped \
                 FROM information_schema.columns \
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                db.string_literal(schema),
                db.string_literal(table)
            ))
        }
        ConnectionKind::MySqlWire => Some(format!(
            "SELECT column_name, data_type, (is_nullable = 'YES') AS is_nullable, \
             (extra LIKE '%auto_increment%' OR extra LIKE '%GENERATED%') AS is_skipped \
             FROM information_schema.columns \
             WHERE table_schema = COALESCE(NULLIF({}, ''), DATABASE()) AND table_name = {} \
             ORDER BY ordinal_position",
            db.string_literal(schema),
            db.string_literal(table)
        )),
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => Some(format!(
            "SELECT name AS column_name, type AS data_type, (\"notnull\" = 0) AS is_nullable, \
             (pk = 1 AND UPPER(type) = 'INTEGER') AS is_skipped \
             FROM pragma_table_info({})",
            db.string_literal(table)
        )),
        // DuckDB's catalog function reports declared types verbatim; a
        // sequence-backed default (`nextval('…')`) is its auto-increment.
        ConnectionKind::DuckDbFile => {
            let schema = if schema.is_empty() { "main" } else { schema };
            Some(format!(
                "SELECT column_name, data_type, is_nullable, \
                 (COALESCE(column_default, '') LIKE 'nextval(%') AS is_skipped \
                 FROM duckdb_columns() \
                 WHERE database_name = current_database() AND schema_name = {} AND table_name = {} \
                 ORDER BY column_index",
                db.string_literal(schema),
                db.string_literal(table)
            ))
        }
        // MATERIALIZED and ALIAS columns are computed by the server; DEFAULT
        // columns still accept explicit values.
        ConnectionKind::ClickHouseHttp => Some(format!(
            "SELECT name AS column_name, type AS data_type, startsWith(type, 'Nullable(') AS is_nullable, \
             default_kind IN ('MATERIALIZED', 'ALIAS') AS is_skipped \
             FROM system.columns WHERE database = {} AND table = {} ORDER BY position",
            if schema.is_empty() {
                "currentDatabase()".to_string()
            } else {
                db.string_literal(schema)
            },
            db.string_literal(table)
        )),
        // Oracle keeps names as stored (upper case unless quoted), so the
        // explorer's schema/table text goes in verbatim. The declared type is
        // re-spelled with its length / precision / scale so the generator
        // can bound values; identity and virtual columns are skipped; the
        // flags are 1/0 because Oracle SQL has no boolean.
        ConnectionKind::OracleNet => {
            let owner = if schema.is_empty() {
                "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".to_string()
            } else {
                db.string_literal(schema)
            };
            Some(format!(
                "SELECT column_name, \
                 CASE \
                 WHEN data_type IN ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR') THEN data_type || '(' || char_length || ')' \
                 WHEN data_type = 'RAW' THEN data_type || '(' || data_length || ')' \
                 WHEN data_type = 'NUMBER' AND data_precision IS NOT NULL AND NVL(data_scale, 0) = 0 THEN data_type || '(' || data_precision || ')' \
                 WHEN data_type = 'NUMBER' AND data_precision IS NOT NULL THEN data_type || '(' || data_precision || ',' || data_scale || ')' \
                 WHEN data_type = 'NUMBER' AND data_scale = 0 THEN 'NUMBER(38)' \
                 ELSE data_type END AS data_type, \
                 CASE WHEN nullable = 'Y' THEN 1 ELSE 0 END AS is_nullable, \
                 CASE WHEN identity_column = 'YES' OR virtual_column = 'YES' THEN 1 ELSE 0 END AS is_skipped \
                 FROM all_tab_cols \
                 WHERE owner = {owner} AND table_name = {} AND hidden_column = 'NO' \
                 ORDER BY column_id",
                db.string_literal(table)
            ))
        }
        _ => None,
    }
}

fn cell_truthy(value: Option<&CellValue>) -> bool {
    matches!(value, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
        || matches!(value, Some(CellValue::String(v)) if v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Parse the metadata result rows (column_name, data_type, is_nullable,
/// is_skipped) into specs with inferred generators.
pub fn parse_columns(rows: &[Vec<CellValue>]) -> Vec<(ColumnSpec, ValueGenerator)> {
    rows.iter()
        .filter_map(|row| {
            let name = match row.first()? {
                CellValue::String(name) if !name.trim().is_empty() => name.clone(),
                _ => return None,
            };
            let data_type = match row.get(1) {
                Some(CellValue::String(ty)) => ty.clone(),
                _ => String::new(),
            };
            let (choices, fixed_literal) = column_extras(&data_type);
            let spec = ColumnSpec {
                nullable: cell_truthy(row.get(2)),
                skip: cell_truthy(row.get(3)),
                name,
                data_type,
                choices,
                fixed_literal,
            };
            let generator = infer_generator(&spec.name, &spec.data_type);
            Some((spec, generator))
        })
        .collect()
}

impl EventEmitter<ToolDialogEvent> for TestDataGeneratorPane {}

pub struct TestDataGeneratorPane {
    context: ToolDialogContext,
    inputs: [Entity<InputState>; 5],
    values: [String; 5],
    columns: Vec<(ColumnSpec, ValueGenerator)>,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    frame: DialogFrame,
    _subscriptions: Vec<gpui::Subscription>,
    focus_handle: FocusHandle,
}

const INPUT_LABELS: [&str; 5] = [
    "explorer.test_data.schema_label",
    "explorer.test_data.table_label",
    "explorer.test_data.rows_label",
    "explorer.test_data.null_percent_label",
    "explorer.test_data.seed_label",
];

impl TestDataGeneratorPane {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let defaults = ["", "", "100", "10", "1"];
        let inputs: [Entity<InputState>; 5] = std::array::from_fn(|index| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx);
                state.set_value(defaults[index], window, cx);
                state
            })
        });
        let mut subscriptions = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.values[index] = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ));
        }
        Self {
            context,
            inputs,
            values: std::array::from_fn(|index| defaults[index].to_string()),
            columns: Vec::new(),
            error: None,
            loading: false,
            refresh_generation: 0,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(560.0)),
            _subscriptions: subscriptions,
            focus_handle: cx.focus_handle(),
        }
    }

    fn load_columns(&mut self, cx: &mut gpui::Context<Self>) {
        let table = self.values[1].trim().to_string();
        if table.is_empty() {
            self.error = Some(crate::localization::text(
                "explorer.test_data.table_required",
            ));
            cx.notify();
            return;
        }
        let Some(query) = columns_query(
            self.context.profile.database_type,
            self.values[0].trim(),
            &table,
        ) else {
            self.error = Some(crate::localization::format(
                "explorer.test_data.unsupported_engine",
                &[("engine", &self.context.profile.database_type.display_name())],
            ));
            cx.notify();
            return;
        };
        self.loading = true;
        self.error = None;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        crate::tool_dialog::run_query(&self.context, query, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.loading = false;
            if let Some(err) = outcome.error {
                this.error = Some(err);
                this.columns = Vec::new();
            } else {
                let set = outcome.sets.into_iter().next().unwrap_or_default();
                this.columns = parse_columns(&set.rows);
                this.error = this
                    .columns
                    .is_empty()
                    .then(|| crate::localization::text("explorer.test_data.no_columns"));
            }
            cx.notify();
        });
        cx.notify();
    }

    fn cycle_generator(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        if let Some((_, generator)) = self.columns.get_mut(index) {
            *generator = generator.next();
            cx.notify();
        }
    }

    fn script(&mut self, cx: &mut gpui::Context<Self>) {
        let row_count = self.values[2].trim().parse::<usize>().unwrap_or(0);
        if row_count == 0 {
            self.emit_warning(
                &crate::localization::text("explorer.test_data.rows_positive"),
                cx,
            );
            return;
        }
        let null_percent = self.values[3].trim().parse::<u8>().unwrap_or(10).min(100);
        let seed = self.values[4].trim().parse::<u64>().unwrap_or(1);
        match insert_script(
            self.context.profile.database_type,
            self.values[0].trim(),
            self.values[1].trim(),
            &self.columns,
            row_count,
            null_percent,
            seed,
        ) {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::text("explorer.test_data.scripted"),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(message) => self.emit_warning(&message, cx),
        }
    }

    fn emit_warning(&mut self, message: &str, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Status {
            level: crate::app::StatusLevel::Warning,
            message: message.to_string(),
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for TestDataGeneratorPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TestDataGeneratorPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let mut form = div().flex().flex_col().gap(px(s(7.0))).p(px(s(10.0)));
        for (index, label) in INPUT_LABELS.iter().enumerate() {
            form = form.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(220.0)))
                            .flex_shrink_0()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .child(crate::localization::text(label)),
                    )
                    .child(div().flex_1().child(Input::new(&self.inputs[index]))),
            );
        }

        let mut column_list = div()
            .id("test-data-columns")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .px(px(s(10.0)));
        if let Some(err) = &self.error {
            column_list = column_list.child(
                div()
                    .p(px(s(6.0)))
                    .text_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .whitespace_normal()
                    .child(err.clone()),
            );
        } else if self.loading {
            column_list =
                column_list.child(div().p(px(s(6.0))).text_color(Theme::text_dim()).child(
                    crate::localization::text("explorer.test_data.loading_columns"),
                ));
        } else if self.columns.is_empty() {
            column_list = column_list.child(
                div()
                    .p(px(s(6.0)))
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(crate::localization::text("explorer.test_data.load_prompt")),
            );
        }
        for (index, (spec, generator)) in self.columns.iter().enumerate() {
            let label = if spec.skip {
                crate::localization::text("explorer.test_data.skipped")
            } else {
                generator.label()
            };
            column_list = column_list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .py(px(s(2.0)))
                    .child(
                        div()
                            .w(px(s(200.0)))
                            .flex_shrink_0()
                            .text_color(Theme::text())
                            .child(spec.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(s(140.0)))
                            .flex_shrink_0()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .child(if spec.nullable {
                                crate::localization::format(
                                    "explorer.test_data.type_nullable",
                                    &[("type", &spec.data_type)],
                                )
                            } else {
                                spec.data_type.clone()
                            }),
                    )
                    .child(if spec.skip {
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .child(label)
                            .into_any_element()
                    } else {
                        Button::new(("test-data-generator", index))
                            .small()
                            .label(label)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.cycle_generator(index, cx)
                            }))
                            .into_any_element()
                    }),
            );
        }

        let focus_handle = self.focus_handle.clone();
        Dialog::new("test-data", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.test_data.title",
                &[("connection", &self.context.profile.name)],
            ))
            .key_context("TestDataGeneratorPane")
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            // The column list scrolls itself; keep it out of the chrome's
            // scroll container.
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .font(Theme::ui_font())
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .p(px(s(14.0)))
                    .child(form)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .border_1()
                            .border_color(Theme::border())
                            .rounded(px(s(4.0)))
                            .flex()
                            .flex_col()
                            .child(column_list),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.test_data.review_explanation",
                            )),
                    ),
            )
            .leading_button(
                Button::new("test-data-load")
                    .label(crate::localization::text("explorer.test_data.load_button"))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.load_columns(cx))),
            )
            .button(
                Button::new("test-data-script")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.test_data.script_button",
                    ))
                    .disabled(self.columns.iter().all(|(spec, _)| spec.skip))
                    .on_click(cx.listener(|this, _, _window, cx| this.script(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, ty: &str, nullable: bool, skip: bool) -> (ColumnSpec, ValueGenerator) {
        let (choices, fixed_literal) = column_extras(ty);
        let spec = ColumnSpec {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            skip,
            choices,
            fixed_literal,
        };
        let generator = infer_generator(&spec.name, &spec.data_type);
        (spec, generator)
    }

    #[test]
    fn clickhouse_types_infer_generators_inside_wrappers() {
        assert_eq!(infer_generator("id", "UInt32"), ValueGenerator::Integer);
        assert_eq!(
            infer_generator("n", "Nullable(Int64)"),
            ValueGenerator::Integer
        );
        assert_eq!(
            infer_generator("code", "LowCardinality(String)"),
            ValueGenerator::Word
        );
        assert_eq!(
            infer_generator("amount", "Decimal(12, 2)"),
            ValueGenerator::Decimal
        );
        assert_eq!(infer_generator("ratio", "Float64"), ValueGenerator::Decimal);
        assert_eq!(
            infer_generator("created", "DateTime64(3, 'UTC')"),
            ValueGenerator::DateTime
        );
        assert_eq!(infer_generator("day", "Date32"), ValueGenerator::Date);
        assert_eq!(infer_generator("flag", "Bool"), ValueGenerator::Boolean);
        assert_eq!(infer_generator("key", "UUID"), ValueGenerator::Guid);
        assert_eq!(
            infer_generator("code", "FixedString(4)"),
            ValueGenerator::Word
        );
        assert_eq!(type_limits("FixedString(4)").max_chars, Some(4));
        assert_eq!(type_limits("Decimal(12, 2)").decimal, Some((12, 2)));
        assert_eq!(type_limits("UInt8").max_int, Some(255));
        assert_eq!(type_limits("Nullable(UInt16)").max_int, Some(65_535));
        assert_eq!(type_limits("int8").max_int, None, "PostgreSQL bigint");
        assert_eq!(
            type_limits_for(Some(DatabaseType::ClickHouse), "Int8").max_int,
            Some(127)
        );
        assert_eq!(
            enum_members("Enum8('sad' = 1, 'ok' = 2, 'it''s' = 3)"),
            vec!["sad", "ok", "it's"]
        );
        assert!(enum_members("String").is_empty());
        assert_eq!(column_extras("Array(String)").1.as_deref(), Some("[]"));
        assert_eq!(
            column_extras("Map(String, UInt8)").1.as_deref(),
            Some("map()")
        );
        assert!(!column_extras("IPv4").0.is_empty());
    }

    #[test]
    fn clickhouse_script_respects_enums_collections_and_small_ints() {
        let columns = vec![
            spec("id", "UInt8", false, false),
            spec(
                "mood",
                "Enum8('sad' = 1, 'ok' = 2, 'happy' = 3)",
                false,
                false,
            ),
            spec("tags", "Array(String)", false, false),
            spec("attrs", "Map(String, String)", false, false),
            spec("ip", "IPv4", false, false),
            spec("name_upper", "String", false, true),
        ];
        let script =
            insert_script(DatabaseType::ClickHouse, "shop", "t", &columns, 20, 0, 7).unwrap();
        assert!(
            script.contains("INSERT INTO `shop`.`t` (`id`, `mood`, `tags`, `attrs`, `ip`) VALUES")
        );
        assert!(!script.contains("name_upper"));
        let mut rows = 0;
        for line in script.lines().filter(|l| l.trim_start().starts_with('(')) {
            rows += 1;
            let inner = line.trim().trim_start_matches('(');
            let mut parts = inner.splitn(5, ", ");
            let id: u64 = parts.next().unwrap().parse().unwrap();
            assert!(id <= 255, "{line}");
            let mood = parts.next().unwrap();
            assert!(matches!(mood, "'sad'" | "'ok'" | "'happy'"), "{line}");
            assert_eq!(parts.next().unwrap(), "[]", "{line}");
            assert_eq!(parts.next().unwrap(), "map()", "{line}");
            assert!(parts.next().unwrap().starts_with('\''), "{line}");
        }
        assert_eq!(rows, 20);
        let sql = columns_query(DatabaseType::ClickHouse, "", "we'ird").unwrap();
        assert!(sql.contains("FROM system.columns"), "{sql}");
        assert!(sql.contains("database = currentDatabase()"), "{sql}");
        assert!(sql.contains("'we''ird'"), "{sql}");
        assert!(
            sql.contains("default_kind IN ('MATERIALIZED', 'ALIAS')"),
            "{sql}"
        );
        let scoped = columns_query(DatabaseType::ClickHouse, "shop", "t").unwrap();
        assert!(scoped.contains("database = 'shop'"), "{scoped}");
    }

    #[test]
    fn inference_matches_types_and_names() {
        assert_eq!(
            infer_generator("Id", "uniqueidentifier"),
            ValueGenerator::Guid
        );
        assert_eq!(infer_generator("Active", "bit"), ValueGenerator::Boolean);
        assert_eq!(
            infer_generator("CreatedAt", "datetime2"),
            ValueGenerator::DateTime
        );
        assert_eq!(infer_generator("BirthDate", "date"), ValueGenerator::Date);
        assert_eq!(infer_generator("Qty", "bigint"), ValueGenerator::Integer);
        assert_eq!(infer_generator("Price", "decimal"), ValueGenerator::Decimal);
        assert_eq!(infer_generator("Email", "nvarchar"), ValueGenerator::Email);
        assert_eq!(infer_generator("FullName", "varchar"), ValueGenerator::Name);
        assert_eq!(infer_generator("Notes", "text"), ValueGenerator::Text);
        assert_eq!(infer_generator("Sku", "varchar"), ValueGenerator::Word);
    }

    #[test]
    fn infer_generator_uses_exact_type_names() {
        assert_eq!(infer_generator("Dur", "interval"), ValueGenerator::Word);
        assert_eq!(
            infer_generator("Dur", "pg_catalog.interval"),
            ValueGenerator::Word
        );
        assert_eq!(
            infer_generator("Flags", "bit varying"),
            ValueGenerator::Word
        );
        assert_eq!(infer_generator("Flags", "varbit"), ValueGenerator::Word);
        assert_eq!(infer_generator("Qty", "int"), ValueGenerator::Integer);
        assert_eq!(infer_generator("Qty", "integer"), ValueGenerator::Integer);
        assert_eq!(infer_generator("Id", "serial"), ValueGenerator::Integer);
        assert_eq!(infer_generator("Active", "bit"), ValueGenerator::Boolean);
        assert_eq!(
            infer_generator("Active", "tinyint(1)"),
            ValueGenerator::Boolean
        );
        assert_eq!(infer_generator("Loc", "point"), ValueGenerator::Word);
    }

    #[test]
    fn infers_and_generates_json_and_binary() {
        assert_eq!(infer_generator("Payload", "json"), ValueGenerator::Json);
        assert_eq!(infer_generator("Doc", "jsonb"), ValueGenerator::Json);
        assert_eq!(
            infer_generator("Blob", "varbinary(max)"),
            ValueGenerator::Binary
        );
        assert_eq!(infer_generator("Data", "bytea"), ValueGenerator::Binary);

        // JSON renders as a quoted literal wrapping valid JSON (Postgres uses
        // plain single quotes, so the inner text is exactly the JSON).
        let mut rng = Rng::new(42);
        let json = generated_value(
            DatabaseType::Postgres,
            ValueGenerator::Json,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(
            json.starts_with('\'') && json.ends_with('\''),
            "expected a quoted literal: {json}"
        );
        let inner = &json[1..json.len() - 1];
        assert!(
            serde_json::from_str::<serde_json::Value>(inner).is_ok(),
            "generated JSON is not valid: {json}"
        );

        // Binary renders as the engine's binary literal.
        let mut rng = Rng::new(42);
        let tds = generated_value(
            DatabaseType::SqlServer,
            ValueGenerator::Binary,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(tds.starts_with("0x"), "{tds}");
        let mut rng = Rng::new(42);
        let sqlite = generated_value(
            DatabaseType::Sqlite,
            ValueGenerator::Binary,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(
            sqlite.starts_with("X'") && sqlite.ends_with('\''),
            "{sqlite}"
        );
    }

    #[test]
    fn same_seed_scripts_identical_data() {
        let columns = vec![
            spec("Name", "nvarchar", false, false),
            spec("Qty", "int", false, false),
        ];
        let a = insert_script(DatabaseType::SqlServer, "dbo", "T", &columns, 20, 10, 7).unwrap();
        let b = insert_script(DatabaseType::SqlServer, "dbo", "T", &columns, 20, 10, 7).unwrap();
        assert_eq!(a, b);
        let c = insert_script(DatabaseType::SqlServer, "dbo", "T", &columns, 20, 10, 8).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn script_uses_dialect_quoting_and_skips_identity() {
        let columns = vec![
            spec("Id", "int", false, true),
            spec("FullName", "nvarchar", false, false),
        ];
        let sql =
            insert_script(DatabaseType::SqlServer, "dbo", "Orders", &columns, 2, 0, 1).unwrap();
        assert!(
            sql.contains("INSERT INTO [dbo].[Orders] ([FullName]) VALUES"),
            "{sql}"
        );
        assert!(!sql.contains("[Id]"), "{sql}");
        assert!(sql.contains("N'"), "{sql}");

        let sql = insert_script(
            DatabaseType::Postgres,
            "public",
            "orders",
            &columns,
            2,
            0,
            1,
        )
        .unwrap();
        assert!(
            sql.contains("INSERT INTO \"public\".\"orders\" (\"FullName\") VALUES"),
            "{sql}"
        );
    }

    #[test]
    fn generated_cell_types_match_generator() {
        let limits = TypeLimits::default();
        let mut rng = Rng::new(1);
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Integer,
                limits,
                &mut rng
            ),
            CellValue::I64(_)
        ));
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Decimal,
                limits,
                &mut rng
            ),
            CellValue::F64(_)
        ));
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Boolean,
                limits,
                &mut rng
            ),
            CellValue::Bool(_)
        ));
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Binary,
                limits,
                &mut rng
            ),
            CellValue::Bytes(_)
        ));
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Word,
                limits,
                &mut rng
            ),
            CellValue::String(_)
        ));
        assert!(matches!(
            generated_cell(
                DatabaseType::SqlServer,
                ValueGenerator::Guid,
                limits,
                &mut rng
            ),
            CellValue::String(_)
        ));
    }

    #[test]
    fn nullable_columns_go_null_at_full_rate() {
        let columns = vec![spec("MiddleName", "nvarchar", true, false)];
        let sql = insert_script(DatabaseType::Sqlite, "", "people", &columns, 5, 100, 3).unwrap();
        assert_eq!(sql.matches("(NULL)").count(), 5, "{sql}");
    }

    #[test]
    fn all_skipped_columns_is_an_error_and_batches_split() {
        let columns = vec![spec("Id", "int", false, true)];
        assert!(insert_script(DatabaseType::MySql, "", "t", &columns, 5, 0, 1).is_err());

        let columns = vec![spec("Qty", "int", false, false)];
        let sql = insert_script(DatabaseType::MySql, "", "t", &columns, 1200, 0, 1).unwrap();
        assert_eq!(sql.matches("INSERT INTO `t`").count(), 3, "{sql}");
    }

    #[test]
    fn columns_queries_are_escaped_and_engine_native() {
        let sql = columns_query(DatabaseType::SqlServer, "", "O'Brien").unwrap();
        assert!(sql.contains("OBJECT_ID(N'dbo.O''Brien')"), "{sql}");
        let sql = columns_query(DatabaseType::Postgres, "", "t").unwrap();
        assert!(sql.contains("table_schema = 'public'"), "{sql}");
        let sql = columns_query(DatabaseType::MySql, "", "t").unwrap();
        assert!(sql.contains("DATABASE()"), "{sql}");
        let sql = columns_query(DatabaseType::Sqlite, "", "t").unwrap();
        assert!(sql.contains("pragma_table_info('t')"), "{sql}");
        let sql = columns_query(DatabaseType::DuckDb, "", "O'Brien").unwrap();
        assert!(sql.contains("duckdb_columns()"), "{sql}");
        assert!(sql.contains("schema_name = 'main'"), "{sql}");
        assert!(sql.contains("table_name = 'O''Brien'"), "{sql}");
        let sql = columns_query(DatabaseType::DuckDb, "analytics", "t").unwrap();
        assert!(sql.contains("schema_name = 'analytics'"), "{sql}");
        assert!(columns_query(DatabaseType::Db2, "", "t").is_none());
    }

    #[test]
    fn oracle_columns_query_reads_all_tab_cols_with_respelled_types() {
        let sql = columns_query(DatabaseType::Oracle, "", "O'HARA").unwrap();
        assert!(sql.contains("FROM all_tab_cols"), "{sql}");
        assert!(
            sql.contains("owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')"),
            "{sql}"
        );
        assert!(sql.contains("table_name = 'O''HARA'"), "{sql}");
        assert!(sql.contains("hidden_column = 'NO'"), "{sql}");
        assert!(
            sql.contains("identity_column = 'YES' OR virtual_column = 'YES'"),
            "{sql}"
        );
        assert!(sql.contains("char_length"), "{sql}");
        assert!(sql.contains("data_precision || ',' || data_scale"), "{sql}");
        assert!(sql.contains("ORDER BY column_id"), "{sql}");
        // An explicit schema is the owner, passed as stored (not re-cased).
        let sql = columns_query(DatabaseType::Oracle, "Hr", "EMP").unwrap();
        assert!(sql.contains("owner = 'Hr'"), "{sql}");
        assert!(!sql.contains("CURRENT_SCHEMA"), "{sql}");
    }

    #[test]
    fn oracle_types_classify_by_number_precision_and_scale() {
        assert_eq!(
            infer_generator("Active", "NUMBER(1)"),
            ValueGenerator::Boolean
        );
        assert_eq!(
            infer_generator("Active", "NUMBER(1,0)"),
            ValueGenerator::Boolean
        );
        assert_eq!(
            infer_generator("Qty", "NUMBER(10)"),
            ValueGenerator::Integer
        );
        assert_eq!(
            infer_generator("Qty", "NUMBER(10,0)"),
            ValueGenerator::Integer
        );
        assert_eq!(
            infer_generator("Qty", "NUMBER(38)"),
            ValueGenerator::Integer
        );
        assert_eq!(
            infer_generator("Amt", "NUMBER(10,2)"),
            ValueGenerator::Decimal
        );
        assert_eq!(infer_generator("Amt", "NUMBER"), ValueGenerator::Decimal);
        assert_eq!(
            infer_generator("Amt", "FLOAT(126)"),
            ValueGenerator::Decimal
        );
        assert_eq!(
            infer_generator("X", "BINARY_DOUBLE"),
            ValueGenerator::Decimal
        );
        assert_eq!(
            infer_generator("X", "BINARY_FLOAT"),
            ValueGenerator::Decimal
        );
        assert_eq!(infer_generator("Sku", "VARCHAR2(20)"), ValueGenerator::Word);
        assert_eq!(
            infer_generator("Email", "NVARCHAR2(80)"),
            ValueGenerator::Email
        );
        assert_eq!(infer_generator("Notes", "CLOB"), ValueGenerator::Text);
        assert_eq!(infer_generator("Born", "DATE"), ValueGenerator::Date);
        assert_eq!(
            infer_generator("Placed", "TIMESTAMP(6)"),
            ValueGenerator::DateTime
        );
        assert_eq!(
            infer_generator("Placed", "TIMESTAMP(6) WITH TIME ZONE"),
            ValueGenerator::DateTime
        );
        // RAW(16) is bytes, never a GUID string — a 36-char string would not
        // be valid RAW input.
        assert_eq!(infer_generator("Id", "RAW(16)"), ValueGenerator::Binary);
        assert_eq!(infer_generator("Doc", "BLOB"), ValueGenerator::Binary);
        assert_eq!(infer_generator("Doc", "LONG RAW"), ValueGenerator::Binary);
        assert_eq!(infer_generator("Doc", "JSON"), ValueGenerator::Json);

        assert_eq!(type_limits("VARCHAR2(20)").max_chars, Some(20));
        assert_eq!(type_limits("RAW(16)").max_bytes, Some(16));
        assert_eq!(type_limits("NUMBER(10,2)").decimal, Some((10, 2)));
        assert_eq!(type_limits("NUMBER(3)").decimal, Some((3, 0)));
        assert_eq!(type_limits("NUMBER").decimal, None);
    }

    #[test]
    fn oracle_literals_use_typed_date_and_timestamp_and_hextoraw() {
        let mut rng = Rng::new(7);
        let date = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Date,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(date.starts_with("DATE '") && date.ends_with('\''), "{date}");
        assert_eq!(date.len(), "DATE 'YYYY-MM-DD'".len(), "{date}");
        let stamp = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::DateTime,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(stamp.starts_with("TIMESTAMP '"), "{stamp}");
        assert_eq!(
            stamp.len(),
            "TIMESTAMP 'YYYY-MM-DD HH:MI:SS'".len(),
            "{stamp}"
        );
        // Cycled onto a character column, the date stays a plain string
        // that fits the declared width.
        let text = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Date,
            TypeLimits {
                max_chars: Some(8),
                ..TypeLimits::default()
            },
            &mut rng,
        );
        assert!(text.starts_with('\'') && !text.contains("DATE"), "{text}");
        assert_eq!(text.len(), 10, "{text}");
        let time = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Time,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(time.starts_with('\'') && time.len() == 10, "{time}");
        let bin = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Binary,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(
            bin.starts_with("HEXTORAW('") && bin.ends_with("')"),
            "{bin}"
        );
        let flag = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Boolean,
            TypeLimits::default(),
            &mut rng,
        );
        assert!(flag == "1" || flag == "0", "{flag}");
        // Other engines keep their string-shaped dates.
        let pg = generated_value(
            DatabaseType::Postgres,
            ValueGenerator::DateTime,
            TypeLimits::default(),
            &mut Rng::new(7),
        );
        assert!(pg.starts_with('\'') && !pg.contains("TIMESTAMP"), "{pg}");
    }

    #[test]
    fn generated_integers_fit_a_small_declared_precision() {
        let limits = type_limits("NUMBER(3)");
        let mut rng = Rng::new(3);
        for _ in 0..200 {
            let value: i64 = generated_value(
                DatabaseType::Oracle,
                ValueGenerator::Integer,
                limits,
                &mut rng,
            )
            .parse()
            .unwrap();
            assert!((0..1000).contains(&value), "{value}");
            match generated_cell(
                DatabaseType::Oracle,
                ValueGenerator::Integer,
                limits,
                &mut rng,
            ) {
                CellValue::I64(n) => assert!((0..1000).contains(&n), "{n}"),
                other => panic!("{other:?}"),
            }
        }
        // Without a declared precision the range is unchanged.
        let mut rng = Rng::new(3);
        let value: i64 = generated_value(
            DatabaseType::Oracle,
            ValueGenerator::Integer,
            TypeLimits::default(),
            &mut rng,
        )
        .parse()
        .unwrap();
        assert!((0..100_000).contains(&value));
    }

    #[test]
    fn oracle_script_uses_union_all_select_batches() {
        let columns = vec![
            spec("ID", "NUMBER(10)", false, true),
            spec("NAME", "VARCHAR2(30)", false, false),
            spec("PLACED", "TIMESTAMP(6)", true, false),
        ];
        let sql = insert_script(DatabaseType::Oracle, "HR", "ORDERS", &columns, 3, 0, 1).unwrap();
        assert!(
            sql.contains("INSERT INTO \"HR\".\"ORDERS\" (\"NAME\", \"PLACED\")\n  SELECT "),
            "{sql}"
        );
        assert_eq!(sql.matches("  UNION ALL SELECT ").count(), 2, "{sql}");
        assert_eq!(sql.matches(" FROM DUAL\n").count(), 2, "{sql}");
        assert_eq!(sql.matches(" FROM DUAL;\n").count(), 1, "{sql}");
        assert!(!sql.contains("\"ID\""), "{sql}");
        assert!(sql.contains("TIMESTAMP '"), "{sql}");
        assert!(
            !sql.contains("VALUES") && !sql.contains("INSERT ALL"),
            "{sql}"
        );

        // More rows than one statement holds → several INSERT ALL batches.
        let sql = insert_script(
            DatabaseType::Oracle,
            "",
            "T",
            &columns,
            ROWS_PER_STATEMENT + 1,
            0,
            1,
        )
        .unwrap();
        assert_eq!(sql.matches("INSERT INTO \"T\" (").count(), 2, "{sql}");
        assert_eq!(sql.matches(" FROM DUAL;\n").count(), 2, "{sql}");
        assert_eq!(
            sql.matches("SELECT ").count(),
            ROWS_PER_STATEMENT + 1,
            "{sql}"
        );
    }

    #[test]
    fn parse_columns_reads_flags_and_infers() {
        let rows = vec![
            vec![
                CellValue::String("Id".into()),
                CellValue::String("int".into()),
                CellValue::Bool(false),
                CellValue::Bool(true),
            ],
            vec![
                CellValue::String("Email".into()),
                CellValue::String("varchar".into()),
                CellValue::I64(1),
                CellValue::I64(0),
            ],
        ];
        let parsed = parse_columns(&rows);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].0.skip);
        assert!(parsed[1].0.nullable);
        assert_eq!(parsed[1].1, ValueGenerator::Email);
    }

    #[test]
    fn type_limits_parses_string_length() {
        assert_eq!(type_limits("varchar(10)").max_chars, Some(10));
        assert_eq!(type_limits("NVARCHAR(3)").max_chars, Some(3));
        assert_eq!(type_limits("char(1)").max_chars, Some(1));
        // Unbounded / unknown forms carry no character limit.
        assert_eq!(type_limits("varchar(max)").max_chars, None);
        assert_eq!(type_limits("nvarchar(-1)").max_chars, None);
        assert_eq!(type_limits("text").max_chars, None);
        assert_eq!(type_limits("varchar").max_chars, None);
        // A numeric type's args are precision/scale, not a character length.
        assert_eq!(type_limits("decimal(10,2)").max_chars, None);
    }

    #[test]
    fn type_limits_parses_decimal_precision_and_scale() {
        assert_eq!(type_limits("decimal(10,2)").decimal, Some((10, 2)));
        assert_eq!(type_limits("NUMERIC(5)").decimal, Some((5, 0)));
        // Scale can never exceed precision.
        assert_eq!(type_limits("decimal(4,9)").decimal, Some((4, 4)));
        // float/money take no (p,s) in the decimal sense.
        assert_eq!(type_limits("float(24)").decimal, None);
        assert_eq!(type_limits("money").decimal, None);
    }

    #[test]
    fn type_limits_parses_binary_byte_length() {
        assert_eq!(type_limits("varbinary(4)").max_bytes, Some(4));
        assert_eq!(type_limits("binary(2)").max_bytes, Some(2));
        assert_eq!(type_limits("varbinary(max)").max_bytes, None);
        assert_eq!(type_limits("blob").max_bytes, None);
        // Binary args are byte lengths, not character or decimal limits.
        assert_eq!(type_limits("varbinary(4)").max_chars, None);
        assert_eq!(type_limits("varbinary(4)").decimal, None);
    }

    #[test]
    fn generated_binary_respects_declared_byte_length() {
        let limits = type_limits("varbinary(4)");
        let mut rng = Rng::new(11);
        for _ in 0..100 {
            let literal = generated_value(
                DatabaseType::SqlServer,
                ValueGenerator::Binary,
                limits,
                &mut rng,
            );
            // SQL Server binary literal is `0x` + two hex chars per byte.
            let hex = literal.strip_prefix("0x").expect("binary literal");
            assert!(
                hex.len() / 2 <= 4,
                "binary value exceeds varbinary(4): {literal}"
            );
        }
    }

    #[test]
    fn user_cycled_generators_still_respect_column_length() {
        // The user can cycle any generator onto any column; a Guid/Json/Date
        // value on a varchar(8) column must still fit its declared length.
        let limits = type_limits("varchar(8)");
        let mut rng = Rng::new(5);
        for generator in [
            ValueGenerator::Guid,
            ValueGenerator::Json,
            ValueGenerator::Date,
            ValueGenerator::DateTime,
            ValueGenerator::Time,
        ] {
            let literal = generated_value(DatabaseType::Postgres, generator, limits, &mut rng);
            // Postgres string literal is plain single quotes; strip and count.
            let inner = literal.trim_start_matches('\'').trim_end_matches('\'');
            assert!(
                inner.chars().count() <= 8,
                "{generator:?} produced too-long value for varchar(8): {literal}"
            );
        }
    }

    #[test]
    fn generated_strings_respect_declared_length() {
        let limits = type_limits("nvarchar(4)");
        let mut rng = Rng::new(1);
        // Every string family clamps to the column's character limit.
        for generator in [
            ValueGenerator::Name,
            ValueGenerator::Word,
            ValueGenerator::Text,
            ValueGenerator::Email,
        ] {
            for _ in 0..50 {
                let literal = generated_value(DatabaseType::SqlServer, generator, limits, &mut rng);
                // SQL Server literal is `N'…'`; count the inner content chars.
                let inner = literal
                    .trim_start_matches('N')
                    .trim_start_matches('\'')
                    .trim_end_matches('\'');
                assert!(
                    inner.chars().count() <= 4,
                    "{generator:?} produced too-long value: {literal}"
                );
            }
        }
    }

    #[test]
    fn generated_decimals_fit_precision_and_scale() {
        let limits = type_limits("decimal(5,2)"); // max 999.99
        let mut rng = Rng::new(9);
        for _ in 0..500 {
            let v = generated_value(
                DatabaseType::SqlServer,
                ValueGenerator::Decimal,
                limits,
                &mut rng,
            );
            let (int_part, frac_part) = v.split_once('.').expect("scale>0 keeps a fraction");
            assert!(int_part.len() <= 3, "integer part too wide: {v}");
            assert_eq!(frac_part.len(), 2, "scale must be exactly 2: {v}");
            assert!(v.parse::<f64>().unwrap() < 1000.0, "{v}");
        }
    }

    #[test]
    fn generated_decimals_with_zero_scale_are_integers() {
        let limits = type_limits("numeric(4)"); // scale 0
        let mut rng = Rng::new(3);
        for _ in 0..100 {
            let v = generated_value(
                DatabaseType::SqlServer,
                ValueGenerator::Decimal,
                limits,
                &mut rng,
            );
            assert!(!v.contains('.'), "zero-scale value must be integral: {v}");
            assert!(v.parse::<u64>().unwrap() < 10_000, "{v}");
        }
    }

    #[test]
    fn length_bounded_script_never_exceeds_column_width() {
        let columns = vec![spec("Code", "varchar(2)", false, false)];
        let sql = insert_script(DatabaseType::SqlServer, "dbo", "T", &columns, 40, 0, 5).unwrap();
        // No scripted string literal may hold more than two characters.
        for literal in sql.split('\'').skip(1).step_by(2) {
            assert!(
                literal.chars().count() <= 2,
                "value exceeds varchar(2): {literal:?}"
            );
        }
    }
}
