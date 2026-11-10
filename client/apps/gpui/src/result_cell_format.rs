//! Result grid cell type inference and alignment resolution.
//!
//! Port of `ResultGridCellFormatter.swift` — the pure type-inference and
//! alignment logic that maps a column's declared SQL data type to a
//! [`ResultKind`], sniffs ambiguous string values, and resolves the
//! effective per-column format against the user's preferences.
//!
//! All logic is pure and unit-tested (per AGENTS.md test mandate).

use std::collections::HashSet;

use sqlly_core::{CellValue, MaskingOverrides, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::masking_rules::{MaskingRules, should_mask};
use crate::preferences::{
    ColumnFormat, DateFormat, DateTimeZoneMode, EpochFormat, EpochUnit, MaskFormat,
    ResultFormattingPreferences, ResultKind, ResultTextAlignment,
};

#[allow(dead_code)]
/// Temporal sub-kind for date columns, so the date formatter knows whether
/// the column is a date, a clock time, or a date+time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultTemporalKind {
    Date,
    Time,
    DateTime,
    Unknown,
}

#[allow(dead_code)]
/// Resolved format for a single column: either a per-column override (with
/// its own options) or the global default for the inferred kind. Mirrors
/// Swift's `ResolvedColumnFormat`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedColumnFormat<'a> {
    pub kind: ResultKind,
    pub column_key: String,
    pub override_fmt: Option<&'a ColumnFormat>,
    pub defaults: &'a crate::preferences::ResultTypeDefaults,
}

impl<'a> ResolvedColumnFormat<'a> {
    #[allow(dead_code)]
    pub fn date_format(&self) -> &DateFormat {
        self.override_fmt
            .map(|o| &o.date_format)
            .unwrap_or(&self.defaults.date_format)
    }

    #[allow(dead_code)]
    /// Horizontal alignment the grid applies to this column. Follows the
    /// resolved per-kind struct so defaults and overrides stay consistent.
    pub fn alignment(&self) -> ResultTextAlignment {
        match self.kind {
            ResultKind::Date => self.date_format().alignment.clone(),
            ResultKind::Bool => self
                .override_fmt
                .map(|o| o.bool_format.alignment.clone())
                .unwrap_or(self.defaults.bool_format.alignment.clone()),
            ResultKind::Uuid => self
                .override_fmt
                .map(|o| o.uuid_format.alignment.clone())
                .unwrap_or(self.defaults.uuid_format.alignment.clone()),
            ResultKind::Integer => self
                .override_fmt
                .map(|o| o.integer_format.alignment.clone())
                .unwrap_or(self.defaults.integer_format.alignment.clone()),
            ResultKind::Decimal => self
                .override_fmt
                .map(|o| o.decimal_format.alignment.clone())
                .unwrap_or(self.defaults.decimal_format.alignment.clone()),
            ResultKind::Mask | ResultKind::Text | ResultKind::JsonPath | ResultKind::Template => {
                ResultTextAlignment::Left
            }
            ResultKind::Epoch | ResultKind::Number => ResultTextAlignment::Right,
        }
    }
}

#[allow(dead_code)]
/// Resolves the effective format for a column against the user's preferences.
/// When a per-column override exists and is not "inherit" (i.e., its kind is
/// not the Text default), it wins. Otherwise, the inferred kind's defaults
/// apply.
pub fn resolve<'a>(
    prefs: &'a ResultFormattingPreferences,
    column_key: &str,
    inferred_kind: ResultKind,
) -> ResolvedColumnFormat<'a> {
    let override_fmt = prefs.column_overrides.get(column_key);
    let kind = match override_fmt {
        Some(o) if !o.is_inherit() => o.kind,
        _ => inferred_kind,
    };
    ResolvedColumnFormat {
        kind,
        column_key: column_key.to_string(),
        override_fmt,
        defaults: &prefs.defaults,
    }
}

/// Normalizes a raw SQL data type string to its base name: trimmed,
/// lowercased, stripped of parenthesized size/precision, and truncated at
/// the first space. E.g. `"NVARCHAR(255)"` → `"nvarchar"`, `"int identity"` →
/// `"int"`.
pub fn base_column_type_name(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    let unwrapped = strip_type_wrappers(&trimmed);
    let stripped_parens = unwrapped.split('(').next().unwrap_or(unwrapped);
    stripped_parens
        .split(' ')
        .next()
        .unwrap_or(stripped_parens)
        .to_string()
}

/// Peels ClickHouse's `Nullable(…)` / `LowCardinality(…)` wrappers (in any
/// nesting) off an already lower-cased type spelling so the inner type is
/// what gets classified: `nullable(decimal(10, 2))` → `decimal(10, 2)`.
pub(crate) fn strip_type_wrappers(lower: &str) -> &str {
    let mut current = lower.trim();
    loop {
        let inner = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|prefix| current.strip_prefix(prefix))
            .and_then(|rest| rest.strip_suffix(')'));
        match inner {
            Some(inner) => current = inner.trim(),
            None => return current,
        }
    }
}

/// The family a declared SQL type belongs to: the one table behind the
/// grid's formatting kind ([`infer_result_kind_for`]), the WHERE menu's
/// value kind (`where_actions::kind_for_type_name`) and the row editor's
/// binary gating (`cell_actions::is_binary_type_for`), so the three can
/// never disagree about a column. `None` is a spelling the table does not
/// know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFamily {
    Bool,
    Integer,
    Decimal,
    Date,
    Uuid,
    Binary,
}

/// [`TypeFamily`] of a declared type. The engine settles the one name two
/// families share: `timestamp` is SQL Server's binary rowversion and a
/// date-time everywhere else (and with no engine to ask, a date-time);
/// `rowversion` is bytes whatever the result claims, since only SQL Server
/// has it. The whole normalised name ([`type_name_key`]) is looked up
/// first, so `long raw` and `timestamp with time zone` are found; a
/// spelling with a trailing qualifier (`int identity`, `bigint unsigned`)
/// then falls back to its first word.
pub fn type_family(db: Option<DatabaseType>, data_type: &str) -> Option<TypeFamily> {
    type_family_of_key(db, &type_name_key(data_type))
}

/// [`type_family`] of an already normalised name ([`type_name_key`]), for
/// a caller that needs the key for its own fallback too.
pub fn type_family_of_key(db: Option<DatabaseType>, key: &str) -> Option<TypeFamily> {
    let sql_server = db.is_some_and(|db| db.family() == sqlly_lang::sqlmodel::Dialect::SqlServer);
    let lookup = |name: &str| -> Option<TypeFamily> {
        if name.is_empty() {
            return None;
        }
        Some(match name {
            "bit" | "bool" | "boolean" | "bitn" => TypeFamily::Bool,
            // SQL Server / PostgreSQL / MySQL / SQLite / DuckDB / ClickHouse /
            // Oracle integer spellings, including the driver-native names the
            // PostgreSQL (`int4`) and DuckDB (`ubigint`) drivers report.
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "smallserial"
            | "serial" | "bigserial" | "serial2" | "serial4" | "serial8" | "int2" | "int4"
            | "int8" | "int16" | "int32" | "int64" | "int128" | "int256" | "uint8" | "uint16"
            | "uint32" | "uint64" | "uint128" | "uint256" | "utinyint" | "usmallint"
            | "uinteger" | "ubigint" | "hugeint" | "uhugeint" | "year" => TypeFamily::Integer,
            "decimal" | "numeric" | "dec" | "fixed" | "float" | "real" | "double" | "money"
            | "smallmoney" | "double precision" | "float4" | "float8" | "float32" | "float64"
            | "bfloat16" | "decimal32" | "decimal64" | "decimal128" | "decimal256" | "number"
            | "binary_float" | "binary_double" => TypeFamily::Decimal,
            "rowversion" => TypeFamily::Binary,
            "timestamp" if sql_server => TypeFamily::Binary,
            "date"
            | "datetime"
            | "datetime2"
            | "smalldatetime"
            | "datetimeoffset"
            | "time"
            | "timestamp"
            | "timestamptz"
            | "timestamp with time zone"
            | "timestamp without time zone"
            | "time with time zone"
            | "time without time zone"
            | "datetime4"
            | "datetimen"
            | "daten"
            | "timen"
            | "datetimeoffsetn" => TypeFamily::Date,
            "uniqueidentifier" | "uuid" | "guid" => TypeFamily::Uuid,
            "binary" | "varbinary" | "image" | "bytea" | "blob" | "tinyblob" | "mediumblob"
            | "longblob" | "raw" | "long raw" => TypeFamily::Binary,
            _ => return None,
        })
    };
    if let Some(family) = lookup(key) {
        return Some(family);
    }
    // A qualified spelling (`int identity`, `bigint unsigned`) falls back
    // to its first word; a one-word key was already looked up.
    key.split_once(' ').and_then(|(first, _)| lookup(first))
}

/// The name [`type_family`] keys on: lower-cased, ClickHouse's
/// `Nullable(…)` peeled off, every `(…)` size or precision group dropped,
/// and inner whitespace collapsed, with multi-word names kept whole (`long
/// raw`, `double precision`, `timestamp with time zone`). Only the group
/// goes, not what follows it: `timestamp(6) with time zone` keeps its
/// `with time zone`. [`base_column_type_name`] cuts at the first space,
/// which would make `long raw` a `long`.
pub fn type_name_key(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    let unwrapped = strip_type_wrappers(&lower);
    // One pass drops the groups and collapses the whitespace between the
    // words that remain: a run of spaces becomes one, and none is kept at
    // either end.
    let mut key = String::with_capacity(unwrapped.len());
    let mut depth = 0usize;
    let mut pending_space = false;
    for c in unwrapped.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_whitespace() => pending_space = !key.is_empty(),
            c => {
                if pending_space {
                    key.push(' ');
                    pending_space = false;
                }
                key.push(c);
            }
        }
    }
    key
}

/// [`infer_result_kind_for`] with no engine, for tests that only care about
/// the engine-independent families. Production always has the engine the
/// result came from and passes it, so SQL Server's `timestamp` is bytes to
/// the grid as it is to the WHERE menu and the row editor.
#[cfg(test)]
pub fn infer_result_kind_from_column(column: Option<&ResultColumn>) -> ResultKind {
    infer_result_kind_for(None, column)
}

/// Infers the formatting kind from a column descriptor and the engine the
/// result came from, through [`type_family`]. A binary column has no
/// formatting kind of its own and renders as text.
pub fn infer_result_kind_for(
    db: Option<DatabaseType>,
    column: Option<&ResultColumn>,
) -> ResultKind {
    let Some(col) = column else {
        return ResultKind::Text;
    };
    let key = type_name_key(&col.data_type);
    match type_family_of_key(db, &key) {
        Some(TypeFamily::Bool) => ResultKind::Bool,
        Some(TypeFamily::Integer) => ResultKind::Integer,
        Some(TypeFamily::Decimal) => ResultKind::Decimal,
        Some(TypeFamily::Date) => ResultKind::Date,
        Some(TypeFamily::Uuid) => ResultKind::Uuid,
        Some(TypeFamily::Binary) => ResultKind::Text,
        // A container type (`Array(Decimal(10, 2))`, `Map(String, UInt8)`,
        // MySQL `ENUM(…)`/`SET(…)`) is text however numeric its elements:
        // the key dropped the element types, so the fallback must not be
        // asked about them either.
        None if is_container_type_key(&key) => ResultKind::Text,
        // Spellings not listed above (`integer unsigned`, engine-specific
        // aliases): fall back to the codegen type categorizer, asked about
        // the normalised key rather than the raw spelling, for the numeric
        // families only, so a numeric column is still treated as numeric
        // by the grid and the exports.
        None => {
            use crate::codegen_types::SqlTypeCategory as C;
            match crate::codegen_types::categorize(&key) {
                C::Int | C::BigInt => ResultKind::Integer,
                C::Float | C::Decimal => ResultKind::Decimal,
                _ => ResultKind::Text,
            }
        }
    }
}

/// Whether a normalised type key names a container or a value list whose
/// `(…)` group held element types, not a size: never a number itself.
fn is_container_type_key(key: &str) -> bool {
    let head = key.split_once(' ').map_or(key, |(first, _)| first);
    matches!(
        head,
        "array" | "map" | "tuple" | "nested" | "enum" | "enum8" | "enum16" | "set" | "variant"
    )
}

/// True when `column` declares a numeric type (integer or decimal), so a
/// cell that arrives as text — the TDS driver renders `decimal`/`money` to
/// a string to keep full precision, and the PostgreSQL and MySQL text
/// protocols deliver every value as text — should still be exported as a
/// number wherever the target format distinguishes numbers from text.
pub fn column_is_numeric(column: Option<&ResultColumn>) -> bool {
    // The numeric families are the same on every engine, so no engine is
    // needed to ask.
    matches!(
        infer_result_kind_for(None, column),
        ResultKind::Integer | ResultKind::Decimal
    )
}

/// The trimmed text when it is a plain decimal literal — optional sign,
/// digits, optional fraction, optional exponent — that a typed export (XLSX,
/// JSON, a SQL script) can emit verbatim as a number. Deliberately stricter
/// than `f64::from_str`: it rejects `inf`, `NaN`, and anything carrying a
/// currency symbol, grouping separator, or trailing unit, all of which must
/// stay text.
///
/// The original digits are returned rather than a reparsed `f64` so a
/// `decimal(38,10)` keeps every digit it came with. A leading `+` is dropped
/// because neither xsd:double nor JSON accepts it.
pub fn numeric_literal(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix(['-', '+']).unwrap_or(trimmed);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
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
    Some(trimmed.strip_prefix('+').unwrap_or(trimmed))
}

/// The text of a numeric cell in `column` as a bare number literal: the
/// digits of a `String` cell that `numeric_literal` accepts when the column
/// is numeric, `None` otherwise. Typed `I64`/`F64` cells never come through
/// here — callers render those directly.
pub fn numeric_text_in_column<'a>(column: Option<&ResultColumn>, text: &'a str) -> Option<&'a str> {
    if column_is_numeric(column) {
        numeric_literal(text)
    } else {
        None
    }
}

#[allow(dead_code)]
/// Infers the formatting kind from a column descriptor and the actual cell
/// value. The descriptor wins when it carries a clear type tag; value
/// sniffing is only used for ambiguous `string`-typed columns.
pub fn infer_result_kind(
    db: Option<DatabaseType>,
    value: &CellValue,
    column: Option<&ResultColumn>,
) -> ResultKind {
    let column_kind = infer_result_kind_for(db, column);
    if column_kind != ResultKind::Text {
        return column_kind;
    }
    match value {
        CellValue::Bool(_) => ResultKind::Bool,
        CellValue::I64(_) => ResultKind::Integer,
        CellValue::F64(_) => ResultKind::Decimal,
        CellValue::String(raw) => sniff_string_kind(raw),
        CellValue::Bytes(_) | CellValue::Null => ResultKind::Text,
    }
}

#[allow(dead_code)]
/// Maps the column's declared SQL data type to the temporal sub-kind so the
/// date formatter knows whether the cell is a date, a clock time, or a
/// date+time.
pub fn temporal_kind(column: Option<&ResultColumn>) -> ResultTemporalKind {
    let Some(col) = column else {
        return ResultTemporalKind::Unknown;
    };
    match base_column_type_name(&col.data_type).as_str() {
        "date" | "daten" => ResultTemporalKind::Date,
        "time" | "timen" => ResultTemporalKind::Time,
        "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "timestamp"
        | "timestamptz" | "datetime4" | "datetimen" | "datetimeoffsetn" => {
            ResultTemporalKind::DateTime
        }
        _ => ResultTemporalKind::Unknown,
    }
}

#[allow(dead_code)]
/// Sniffs strings for clear UUID or date shapes. Conservatively returns
/// `Text` when neither matches.
fn sniff_string_kind(raw: &str) -> ResultKind {
    let trimmed = raw.trim();
    if is_likely_uuid(trimmed) {
        return ResultKind::Uuid;
    }
    if is_likely_date(trimmed) {
        return ResultKind::Date;
    }
    ResultKind::Text
}

#[allow(dead_code)]
/// Detects UUID-like strings: 8-4-4-4-12 hex with dashes, or 32 hex without
/// dashes.
fn is_likely_uuid(raw: &str) -> bool {
    let len = raw.len();
    if len == 36 {
        // 8-4-4-4-12 with dashes
        raw.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
    } else if len == 32 {
        raw.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

#[allow(dead_code)]
/// Detects date-like strings: must contain a date separator (`-` or `/`) and
/// at least one digit. Conservative — rejects pure time strings.
fn is_likely_date(raw: &str) -> bool {
    let has_sep = raw.contains('-') || raw.contains('/');
    let has_digit = raw.chars().any(|c| c.is_ascii_digit());
    has_sep && has_digit
}

#[allow(dead_code)]
/// Resolves the horizontal alignment for a column without a specific cell
/// value (used by the grid header).
pub fn column_alignment(
    db: Option<DatabaseType>,
    column_index: usize,
    metadata: &[ResultColumn],
    prefs: &ResultFormattingPreferences,
    column_key: Option<&str>,
) -> ResultTextAlignment {
    let column = metadata.get(column_index);
    let key = column_key
        .map(|s| s.to_string())
        .unwrap_or_else(|| column.map(|c| c.name.clone()).unwrap_or_default());
    let resolved = resolve(prefs, &key, infer_result_kind_for(db, column));
    resolved.alignment()
}

/// Char-based mask. If prefix+suffix cover the whole string, return it unchanged.
pub fn mask_text(raw: &str, fmt: &MaskFormat) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let len = chars.len();
    if fmt.keep_prefix.saturating_add(fmt.keep_suffix) >= len {
        return raw.to_string();
    }
    let mut out = String::new();
    out.extend(chars.iter().take(fmt.keep_prefix));
    let hidden = len - fmt.keep_prefix - fmt.keep_suffix;
    for _ in 0..hidden {
        out.push(fmt.mask_char);
    }
    if fmt.keep_suffix > 0 {
        out.extend(chars.iter().skip(len - fmt.keep_suffix));
    }
    out
}

/// The display time zone a [`DateTimeZoneMode`] + identifier pair resolves
/// to. `AsStored` means "do not shift the value" — naive values render
/// unchanged and offset-carrying values keep their own offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedZone {
    Utc,
    Local,
    Named(chrono_tz::Tz),
    AsStored,
}

/// A display-zone identifier that is not an IANA time zone name (empty when
/// the identifier was blank). Cheap to build: render paths that hit it fall
/// back to UTC without wording anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTimeZone(pub String);

impl UnknownTimeZone {
    /// The settings-pane explanation, in the active language (UI thread).
    pub fn message(&self) -> String {
        if self.0.is_empty() {
            crate::localization::text("settings.results.message.time_zone_empty")
        } else {
            crate::localization::format(
                "settings.results.message.time_zone_unknown",
                &[("time_zone", self.0.as_str())],
            )
        }
    }
}

/// Resolve a display-zone preference to a concrete [`ResolvedZone`].
/// `AlwaysInTimeZone` parses `identifier` as an IANA name; an empty or
/// unknown identifier is an error so the preferences pane can complain while
/// render paths fall back to UTC.
pub fn resolve_zone(
    mode: DateTimeZoneMode,
    identifier: &str,
) -> Result<ResolvedZone, UnknownTimeZone> {
    match mode {
        DateTimeZoneMode::AlwaysUtc => Ok(ResolvedZone::Utc),
        DateTimeZoneMode::NativeAsStored => Ok(ResolvedZone::AsStored),
        DateTimeZoneMode::AlwaysLocal => Ok(ResolvedZone::Local),
        DateTimeZoneMode::AlwaysInTimeZone => {
            use std::str::FromStr as _;
            let trimmed = identifier.trim();
            if trimmed.is_empty() {
                return Err(UnknownTimeZone(String::new()));
            }
            chrono_tz::Tz::from_str(trimmed)
                .map(ResolvedZone::Named)
                .map_err(|_| UnknownTimeZone(trimmed.to_string()))
        }
    }
}

/// Render a UTC instant in `zone` with a `strftime`-style `pattern`.
/// `AsStored` renders as UTC — callers that must not shift a value are
/// expected to skip the render instead (see [`format_datetime_text`]).
/// On wasm `Local` falls back to UTC (`chrono::Local` is unavailable there).
pub fn render_in_zone(
    utc: chrono::DateTime<chrono::Utc>,
    zone: &ResolvedZone,
    pattern: &str,
) -> String {
    match zone {
        ResolvedZone::Utc | ResolvedZone::AsStored => utc.format(pattern).to_string(),
        ResolvedZone::Local => {
            #[cfg(target_arch = "wasm32")]
            {
                utc.format(pattern).to_string()
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                utc.with_timezone(&chrono::Local)
                    .format(pattern)
                    .to_string()
            }
        }
        ResolvedZone::Named(tz) => utc.with_timezone(tz).format(pattern).to_string(),
    }
}

/// Replace the global Date display zone in `prefs` with a connection's own
/// display zone (`None` keeps the app setting). Column overrides that follow
/// the global zone then follow the connection; pinned ones still win.
///
/// Call it on the per-view effective preferences — never on preferences
/// that get saved, or the connection's zone would become the app's.
pub fn apply_connection_display_zone(
    prefs: &mut ResultFormattingPreferences,
    zone: Option<&sqlly_core::DisplayTimeZone>,
) {
    use sqlly_core::DisplayTimeZone;
    let Some(zone) = zone else {
        return;
    };
    let fmt = &mut prefs.defaults.date_format;
    match zone {
        DisplayTimeZone::Utc => {
            fmt.time_zone_mode = DateTimeZoneMode::AlwaysUtc;
            fmt.time_zone_identifier = "UTC".into();
        }
        DisplayTimeZone::Local => fmt.time_zone_mode = DateTimeZoneMode::AlwaysLocal,
        DisplayTimeZone::Named(name) => {
            fmt.time_zone_mode = DateTimeZoneMode::AlwaysInTimeZone;
            fmt.time_zone_identifier = name.clone();
        }
    }
}

/// The date format a column renders with: the override's pattern and
/// alignment (when the column has an override), in the override's own zone
/// when it pins one, else in the global (or connection) zone.
pub fn effective_override_date_format(
    override_fmt: Option<&ColumnFormat>,
    defaults: &DateFormat,
) -> DateFormat {
    let Some(override_fmt) = override_fmt else {
        return defaults.clone();
    };
    let mut fmt = override_fmt.date_format.clone();
    if fmt.follows_global_zone() {
        fmt.time_zone_mode = defaults.time_zone_mode.clone();
        fmt.time_zone_identifier = defaults.time_zone_identifier.clone();
    }
    fmt
}

/// Pin an epoch format that follows the global zone to `defaults`' zone, so
/// [`epoch_to_datetime`] renders it there.
pub fn resolve_epoch_zone(fmt: &mut EpochFormat, defaults: &DateFormat) {
    if fmt.time_zone_mode.is_none() {
        fmt.time_zone_mode = Some(defaults.time_zone_mode.clone());
        fmt.time_zone_identifier = defaults.time_zone_identifier.clone();
    }
}

/// Words that, as the last word of an integer column's name, mark it as a
/// probable epoch timestamp (`created_at`, `updated_ts`, `event_time`, …).
/// (`_on` and `_date` are left out: `turned_on` flags and `20260101`-style
/// date integers would render as 1970.)
const EPOCH_NAME_SUFFIXES: &[&str] = &[
    "at",
    "ts",
    "time",
    "timestamp",
    "epoch",
    "unixtime",
    "datetime",
];

/// Whole names that read as timestamps on their own.
const EPOCH_WHOLE_NAMES: &[&str] = &[
    "timestamp",
    "ts",
    "time",
    "epoch",
    "unixtime",
    "created",
    "updated",
    "modified",
    "deleted",
    "mtime",
    "ctime",
    "atime",
];

/// Words before `time`/`ts` that make the column a duration or a count, not
/// an instant (`response_time`, `elapsed_time`, `total_ts`).
const DURATION_WORDS: &[&str] = &[
    "elapsed",
    "duration",
    "response",
    "run",
    "runtime",
    "exec",
    "execution",
    "wait",
    "cpu",
    "total",
    "avg",
    "average",
    "max",
    "min",
    "sum",
    "process",
    "processing",
    "up",
    "idle",
    "lead",
    "cycle",
    "ttl",
    "spent",
    "load",
    "build",
    "query",
];

/// Split a column name into lowercase words at `_`, `-`, spaces, dots and
/// camelCase boundaries (`createdAt` → `created`, `at`).
fn name_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.extend(ch.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Whether an integer column's name reads like an epoch timestamp
/// (`created_at`, `updatedAt`, `event_ts`, `login_time`, `timestamp`, …),
/// case-insensitively. Durations (`response_time`, `elapsed_ts`) do not.
pub fn epoch_name_candidate(name: &str) -> bool {
    let words = name_words(name);
    let Some(last) = words.last() else {
        return false;
    };
    if words.len() == 1 {
        return EPOCH_WHOLE_NAMES.contains(&last.as_str());
    }
    if !EPOCH_NAME_SUFFIXES.contains(&last.as_str()) {
        return false;
    }
    let before = &words[words.len() - 2];
    !DURATION_WORDS.contains(&before.as_str())
}

/// 2000-01-01T00:00:00Z and 2100-01-01T00:00:00Z as epoch seconds.
const PLAUSIBLE_EPOCH_SECONDS: std::ops::Range<i64> = 946_684_800..4_102_444_800;

/// Whether `n` is a plausible epoch timestamp: 2000–2100 as seconds or as
/// milliseconds.
pub fn plausible_epoch(n: i64) -> bool {
    PLAUSIBLE_EPOCH_SECONDS.contains(&n)
        || (PLAUSIBLE_EPOCH_SECONDS.start * 1000..PLAUSIBLE_EPOCH_SECONDS.end * 1000).contains(&n)
}

/// Whether a raw cell holds a plausible epoch value (see [`plausible_epoch`]).
pub fn plausible_epoch_cell(value: &CellValue) -> bool {
    match value {
        CellValue::I64(n) => plausible_epoch(*n),
        CellValue::String(s) => s.trim().parse::<i64>().is_ok_and(plausible_epoch),
        _ => false,
    }
}

/// Convert an integer (or numeric string) epoch to a formatted datetime.
pub fn epoch_to_datetime(value: &CellValue, fmt: &EpochFormat) -> Option<String> {
    let n = match value {
        CellValue::I64(v) => *v,
        CellValue::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    let utc_dt = match fmt.unit {
        EpochUnit::Auto => {
            if n.unsigned_abs() >= 100_000_000_000 {
                chrono::DateTime::from_timestamp_millis(n)?
            } else {
                chrono::DateTime::from_timestamp(n, 0)?
            }
        }
        EpochUnit::Seconds => chrono::DateTime::from_timestamp(n, 0)?,
        EpochUnit::Milliseconds => chrono::DateTime::from_timestamp_millis(n)?,
    };
    // Reject implausible timestamps — e.g. a milliseconds value interpreted with
    // the Seconds unit lands in year ~47337, which `%Y` renders as `+47337`.
    // Years outside 1..=9999 signal the wrong unit, so treat them as out of range.
    if !(1..=9999).contains(&chrono::Datelike::year(&utc_dt)) {
        return None;
    }
    // An epoch is an absolute instant, so "as stored" degenerates to UTC and
    // an unresolvable named zone falls back to UTC (the pane reports the
    // error; render paths must still produce something sensible). A format
    // still following the global zone here was not resolved by the caller
    // (see `grid_bridge::column_cell_plan`), so it renders in UTC.
    let mode = fmt
        .time_zone_mode
        .clone()
        .unwrap_or(DateTimeZoneMode::AlwaysUtc);
    let zone = resolve_zone(mode, &fmt.time_zone_identifier).unwrap_or(ResolvedZone::Utc);
    Some(render_in_zone(utc_dt, &zone, &fmt.pattern))
}

/// Re-render a datetime cell's engine text in the column's display time zone
/// and format pattern. Returns `None` when the cell should stay as the engine
/// sent it: unparseable text, date-only or time-only values (shifting them
/// across zones needs a time-of-day the value does not carry), and the
/// `NativeAsStored` mode.
///
/// Values with an explicit offset (`timestamptz`, `datetimeoffset`, ISO
/// strings ending in `Z`/`±hh:mm`) convert exactly; naive values are treated
/// as UTC — matching the long-standing `AlwaysUtc` semantics — and only
/// convert when the mode asks for another zone.
pub fn format_datetime_text(raw: &str, fmt: &DateFormat) -> Option<String> {
    let zone = resolve_zone(fmt.time_zone_mode.clone(), &fmt.time_zone_identifier)
        .unwrap_or(ResolvedZone::Utc);
    if zone == ResolvedZone::AsStored {
        return None;
    }
    let trimmed = raw.trim();
    let utc_dt = parse_datetime_utc(trimmed)?;
    let mut pattern = swift_date_format_to_strftime(&fmt.format);
    // Preserve sub-second fidelity the engine sent: `%.f` prints the
    // fraction with a leading dot and nothing at all when it is zero.
    if chrono::Timelike::nanosecond(&utc_dt) != 0
        && pattern.contains("%S")
        && !pattern.contains("%.f")
        && !pattern.contains("%f")
    {
        pattern = pattern.replace("%S", "%S%.f");
    }
    Some(render_in_zone(utc_dt, &zone, &pattern))
}

/// Render a date or datetime cell's engine text with an explicit
/// `strftime` pattern — the `-- sqlly format: column=…; date=…` directive
/// (which the grid's "Set Formatting Override…" Date choices write).
///
/// Unlike [`format_datetime_text`], date-only values are formatted too (as
/// midnight, never shifted across zones). Datetimes shift into the column's
/// display time zone (`zone_fmt`) the same way the preference-driven path
/// does; under "as stored" an offset-carrying value keeps its own offset.
/// Returns `None` — keep the engine text — for anything unparseable and for
/// a pattern chrono cannot render, so a typo in a directive can never panic
/// the grid.
pub fn format_date_text_with_pattern(
    raw: &str,
    pattern: &str,
    zone_fmt: &DateFormat,
) -> Option<String> {
    let trimmed = raw.trim();
    let zone = resolve_zone(
        zone_fmt.time_zone_mode.clone(),
        &zone_fmt.time_zone_identifier,
    )
    .unwrap_or(ResolvedZone::Utc);
    if let Some(with_offset) = parse_datetime_with_offset(trimmed) {
        let utc = with_offset.with_timezone(&chrono::Utc);
        return match zone {
            ResolvedZone::AsStored => try_render(&with_offset, pattern),
            ResolvedZone::Utc => try_render(&utc, pattern),
            ResolvedZone::Named(tz) => try_render(&utc.with_timezone(&tz), pattern),
            ResolvedZone::Local => render_local(utc, pattern),
        };
    }
    for naive_format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, naive_format) {
            // Naive values are read as UTC, matching the preference path.
            let utc = naive.and_utc();
            return match zone {
                ResolvedZone::AsStored | ResolvedZone::Utc => try_render(&utc, pattern),
                ResolvedZone::Named(tz) => try_render(&utc.with_timezone(&tz), pattern),
                ResolvedZone::Local => render_local(utc, pattern),
            };
        }
    }
    let date = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok()?;
    try_render(&date.and_time(chrono::NaiveTime::MIN).and_utc(), pattern)
}

/// RFC 3339 plus the space-separated `…+02` / `…+02:00` spelling PostgreSQL
/// uses for `timestamptz` text.
fn parse_datetime_with_offset(text: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .or_else(|| chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%#z").ok())
}

/// Format `value` with a user-supplied pattern, or `None` when the pattern
/// holds a specifier chrono rejects (formatting such a pattern through
/// `to_string` would panic).
fn try_render<Tz>(value: &chrono::DateTime<Tz>, pattern: &str) -> Option<String>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    use std::fmt::Write as _;
    let mut out = String::new();
    write!(out, "{}", value.format(pattern)).ok()?;
    Some(out)
}

fn render_local(utc: chrono::DateTime<chrono::Utc>, pattern: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        try_render(&utc, pattern)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        try_render(&utc.with_timezone(&chrono::Local), pattern)
    }
}

/// Translate a Swift-style date format string (the stored preference shape,
/// e.g. `"yyyy-MM-dd HH:mm:ss"`) to `strftime`-style for chrono.
pub fn swift_date_format_to_strftime(swift: &str) -> String {
    swift
        .replace("yyyy", "%Y")
        .replace("MM", "%m")
        .replace("dd", "%d")
        .replace("HH", "%H")
        .replace("mm", "%M")
        .replace("ss", "%S")
}

/// Parse engine datetime text into a UTC instant. Accepts RFC 3339 (what the
/// PG/TDS layers emit for `timestamptz`/`datetimeoffset`) and the naive
/// `YYYY-MM-DD HH:MM:SS[.frac]` / `YYYY-MM-DDTHH:MM:SS[.frac]` shapes naive
/// timestamps stringify to; naive values are interpreted as UTC. Date-only
/// and time-only strings return `None`.
fn parse_datetime_utc(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    for naive_format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, naive_format) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Display-only transform. Other kinds return `None` so the original cell is kept.
pub fn display_transform(value: &CellValue, fmt: &ColumnFormat) -> Option<CellValue> {
    match fmt.kind {
        ResultKind::Mask => {
            if matches!(value, CellValue::Null) {
                return None;
            }
            Some(CellValue::String(mask_text(
                &crate::result_export::cell_display(value),
                &fmt.mask_format,
            )))
        }
        ResultKind::Epoch => epoch_to_datetime(value, &fmt.epoch_format).map(CellValue::String),
        ResultKind::JsonPath => {
            let CellValue::String(raw) = value else {
                return None;
            };
            let fmt_json = &fmt.json_path_format;
            Some(CellValue::String(
                crate::cell_formatters::json_path_extract(raw, &fmt_json.path)
                    .unwrap_or_else(|| fmt_json.missing_text.clone()),
            ))
        }
        ResultKind::Template => {
            if matches!(value, CellValue::Null) {
                return None;
            }
            let shown = crate::result_export::cell_display(value);
            Some(CellValue::String(crate::cell_formatters::render_template(
                &shown,
                &fmt.template_format.template,
            )))
        }
        ResultKind::Number => crate::cell_formatters::numeric_value(value).map(|n| {
            CellValue::String(crate::cell_formatters::format_number_style(
                n,
                &fmt.number_style_format,
            ))
        }),
        _ => None,
    }
}

/// Apply mask overrides to export/copy rows. Epoch stays raw. No-op when the
/// flag is off or no Mask overrides exist.
pub fn apply_export_masks(
    columns: &[ResultColumn],
    rows: Vec<Vec<CellValue>>,
    prefs: &ResultFormattingPreferences,
) -> Vec<Vec<CellValue>> {
    if !prefs.exports_masked {
        return rows;
    }
    let masks: Vec<Option<&ColumnFormat>> = columns
        .iter()
        .map(|c| {
            prefs
                .column_overrides
                .get(&c.name)
                .filter(|f| f.kind == ResultKind::Mask)
        })
        .collect();
    if masks.iter().all(Option::is_none) {
        return rows;
    }
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .enumerate()
                .map(|(i, cell)| {
                    masks
                        .get(i)
                        .copied()
                        .flatten()
                        .and_then(|fmt| display_transform(&cell, fmt))
                        .unwrap_or(cell)
                })
                .collect()
        })
        .collect()
}

/// Where a result column's values come from: the base table's identifier
/// chain (`["Sales", "Orders"]`) and that table's column name. Known only when
/// the executed statement attributes the column with certainty (see
/// `grid_bridge::column_sources`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSource {
    pub table: Vec<String>,
    pub column: String,
}

/// True when the Column overrides key `key` is a qualified `table.column`
/// (or `schema.table.column`) key naming this result column: its qualifier
/// matches the trailing parts of the source table and its last part matches
/// the source column or the result column's own name. Case-insensitive;
/// `[x]`, `"x"`, and `` `x` `` quoting is ignored. A bare key never matches
/// here — bare keys are looked up by name.
pub fn qualified_key_matches(key: &str, column: &ResultColumn, source: &ColumnSource) -> bool {
    fn unquote(part: &str) -> &str {
        let part = part.trim();
        for (open, close) in [('[', ']'), ('"', '"'), ('`', '`')] {
            if let Some(inner) = part
                .strip_prefix(open)
                .and_then(|rest| rest.strip_suffix(close))
            {
                return inner;
            }
        }
        part
    }
    let parts: Vec<&str> = key.split('.').map(unquote).collect();
    let Some((column_part, qualifier)) = parts.split_last() else {
        return false;
    };
    if qualifier.is_empty() || qualifier.len() > source.table.len() {
        return false;
    }
    let table_tail = &source.table[source.table.len() - qualifier.len()..];
    let table_matches = qualifier
        .iter()
        .zip(table_tail)
        .all(|(want, have)| want.eq_ignore_ascii_case(unquote(have)));
    table_matches
        && (column_part.eq_ignore_ascii_case(&source.column)
            || column_part.eq_ignore_ascii_case(&column.name))
}

/// Resolve qualified `table.column` Column overrides against the result's
/// columns. Every consumer (grid formats, cell conversion, copy/export masks)
/// looks overrides up by the result column's name, so a matching qualified
/// override is filed under that name, replacing a bare-name override — the
/// qualified key is the more specific one. `sources[i]` is the source of
/// `columns[i]` when known.
///
/// Result columns that share a name are only rewritten when every one of
/// them resolves to the same qualified override; otherwise the bare-name
/// override (if any) stays in charge for all of them, since the name is the
/// only key downstream lookups have.
pub fn resolve_qualified_overrides(
    prefs: &mut ResultFormattingPreferences,
    columns: &[ResultColumn],
    sources: &[Option<ColumnSource>],
) {
    let qualified_keys: Vec<&String> = prefs
        .column_overrides
        .keys()
        .filter(|key| key.contains('.'))
        .collect();
    if qualified_keys.is_empty() {
        return;
    }
    let matches: Vec<Option<String>> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let source = sources.get(i)?.as_ref()?;
            // Prefer the most specific (longest) qualifier when several match.
            qualified_keys
                .iter()
                .filter(|key| qualified_key_matches(key, column, source))
                .max_by_key(|key| key.split('.').count())
                .map(|key| (*key).clone())
        })
        .collect();
    let mut resolved: std::collections::BTreeMap<String, ColumnFormat> =
        std::collections::BTreeMap::new();
    let mut conflicted: HashSet<&str> = HashSet::new();
    for (column, matched) in columns.iter().zip(&matches) {
        let Some(key) = matched else {
            conflicted.insert(column.name.as_str());
            continue;
        };
        let Some(fmt) = prefs.column_overrides.get(key) else {
            continue;
        };
        match resolved.get(&column.name) {
            Some(existing) if existing != fmt => {
                conflicted.insert(column.name.as_str());
            }
            _ => {
                resolved.insert(column.name.clone(), fmt.clone());
            }
        }
    }
    for (name, fmt) in resolved {
        if !conflicted.contains(name.as_str()) {
            prefs.column_overrides.insert(name, fmt);
        }
    }
}

/// File every `mask=` directive (`-- sqlly format: column=…; mask=…`) as a
/// Mask override under its column's name, so copy and export mask it exactly
/// like a Column-override mask (and honor "Mask exports" the same way). The
/// grid already shows the directive's mask; this makes the clipboard and
/// exported files agree. A directive wins over a configured override, as it
/// does on screen.
pub fn inject_directive_masks(
    prefs: &mut ResultFormattingPreferences,
    columns: &[ResultColumn],
    hints: &[crate::query_format::QueryFormatHint],
) {
    if hints.is_empty() {
        return;
    }
    for column in columns {
        if let Some(fmt) = crate::query_format::display_format_for_column(column, hints)
            .filter(|fmt| fmt.kind == ResultKind::Mask)
        {
            prefs.column_overrides.insert(column.name.clone(), fmt);
        }
    }
}

/// Copy of `prefs` with an auto-mask `ColumnFormat` injected for every column that
/// `masking_rules::should_mask` selects and that has no explicit override, minus the
/// columns in `revealed`. Explicit user overrides always win.
pub fn effective_formatting(
    prefs: &ResultFormattingPreferences,
    rules: &MaskingRules,
    overrides: &MaskingOverrides,
    columns: &[ResultColumn],
    table: Option<&str>,
    revealed: &HashSet<usize>,
) -> ResultFormattingPreferences {
    let mut out = prefs.clone();
    for (i, column) in columns.iter().enumerate() {
        if revealed.contains(&i) {
            continue;
        }
        if !should_mask(rules, overrides, &column.name, table) {
            continue;
        }
        out.column_overrides
            .entry(column.name.clone())
            .or_insert_with(|| ColumnFormat::default_for_kind(ResultKind::Mask));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container type drops its element types with its `(…)` group, so
    /// what is left must not be read as the elements' family: an array of
    /// decimals is not a decimal, and the fallback categorizer is asked
    /// about the normalised key, never the raw spelling.
    #[test]
    fn container_types_are_text_not_their_elements() {
        let col = |t: &str| ResultColumn {
            name: "c".into(),
            data_type: t.into(),
            nullable: true,
        };
        for t in [
            "Array(Decimal(10, 2))",
            "Nullable(Array(Int32))",
            "Map(String, UInt8)",
            "Tuple(Int32, Float64)",
            "enum('a','b')",
            "set('x','y')",
        ] {
            assert_eq!(
                infer_result_kind_for(Some(DatabaseType::ClickHouse), Some(&col(t))),
                ResultKind::Text,
                "{t}"
            );
        }
        assert_eq!(
            infer_result_kind_for(
                Some(DatabaseType::ClickHouse),
                Some(&col("Nullable(Decimal(10, 2))"))
            ),
            ResultKind::Decimal
        );
        assert_eq!(
            infer_result_kind_for(Some(DatabaseType::MySql), Some(&col("int(11) unsigned"))),
            ResultKind::Integer
        );
    }
    use crate::preferences::ResultFormattingPreferences;
    use sqlly_core::{CellValue, ResultColumn};

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    #[test]
    fn base_column_type_name_strips_parens() {
        assert_eq!(base_column_type_name("NVARCHAR(255)"), "nvarchar");
        assert_eq!(base_column_type_name("DECIMAL(10,2)"), "decimal");
        assert_eq!(base_column_type_name("int"), "int");
    }

    #[test]
    fn base_column_type_name_strips_suffix() {
        assert_eq!(base_column_type_name("int identity"), "int");
        assert_eq!(base_column_type_name("bigint NOT NULL"), "bigint");
    }

    #[test]
    fn base_column_type_name_lowercases() {
        assert_eq!(base_column_type_name("VARCHAR(MAX)"), "varchar");
    }

    #[test]
    fn infer_bool_types() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "bit"))),
            ResultKind::Bool
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "bitn"))),
            ResultKind::Bool
        );
    }

    #[test]
    fn infer_integer_types() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "int"))),
            ResultKind::Integer
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "bigint"))),
            ResultKind::Integer
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "smallint"))),
            ResultKind::Integer
        );
    }

    #[test]
    fn infer_driver_native_numeric_spellings() {
        // PostgreSQL wire names, MySQL canonical names, Oracle NUMBER,
        // DuckDB unsigned/huge ints, ClickHouse sized ints and decimals.
        for ty in [
            "int2",
            "int4",
            "int8",
            "mediumint",
            "year",
            "utinyint",
            "ubigint",
            "hugeint",
            "uint64",
            "int128",
            "bigint unsigned",
            "integer unsigned",
        ] {
            assert_eq!(
                infer_result_kind_from_column(Some(&col("c", ty))),
                ResultKind::Integer,
                "{ty}"
            );
        }
        for ty in [
            "float4",
            "float8",
            "number",
            "number(10,2)",
            "binary_double",
            "float64",
            "decimal32(2)",
            "decimal(38, 10)",
            "dec(10,2)",
            "numeric(12,4)",
        ] {
            assert_eq!(
                infer_result_kind_from_column(Some(&col("c", ty))),
                ResultKind::Decimal,
                "{ty}"
            );
        }
    }

    #[test]
    fn infer_unwraps_clickhouse_wrappers() {
        assert_eq!(base_column_type_name("Nullable(Decimal(10, 2))"), "decimal");
        assert_eq!(
            base_column_type_name("LowCardinality(Nullable(UInt64))"),
            "uint64"
        );
        assert_eq!(base_column_type_name("Array(Nullable(Int32))"), "array");
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "Nullable(Decimal(10, 2))"))),
            ResultKind::Decimal
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "Nullable(String)"))),
            ResultKind::Text
        );
    }

    #[test]
    fn numeric_literal_accepts_and_rejects() {
        assert_eq!(numeric_literal(" 42 "), Some("42"));
        assert_eq!(numeric_literal("-1.50"), Some("-1.50"));
        assert_eq!(numeric_literal("+7"), Some("7"));
        assert_eq!(numeric_literal(".5"), Some(".5"));
        assert_eq!(numeric_literal("1e10"), Some("1e10"));
        assert_eq!(numeric_literal("1E-10"), Some("1E-10"));
        assert_eq!(numeric_literal("inf"), None);
        assert_eq!(numeric_literal("NaN"), None);
        assert_eq!(numeric_literal("1,000"), None);
        assert_eq!(numeric_literal("12%"), None);
        assert_eq!(numeric_literal("1e"), None);
        assert_eq!(numeric_literal("-"), None);
        assert_eq!(numeric_literal(""), None);
    }

    #[test]
    fn numeric_text_in_column_requires_a_numeric_column() {
        assert_eq!(
            numeric_text_in_column(Some(&col("c", "decimal(10,2)")), "12.50"),
            Some("12.50")
        );
        assert_eq!(
            numeric_text_in_column(Some(&col("c", "varchar")), "00123"),
            None
        );
        assert_eq!(
            numeric_text_in_column(Some(&col("c", "money")), "$1.00"),
            None
        );
        assert_eq!(numeric_text_in_column(None, "1"), None);
    }

    #[test]
    fn infer_decimal_types() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "decimal(10,2)"))),
            ResultKind::Decimal
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "float"))),
            ResultKind::Decimal
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "money"))),
            ResultKind::Decimal
        );
    }

    #[test]
    fn infer_date_types() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "datetime"))),
            ResultKind::Date
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "datetime2"))),
            ResultKind::Date
        );
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "date"))),
            ResultKind::Date
        );
    }

    #[test]
    fn infer_uuid_types() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "uniqueidentifier"))),
            ResultKind::Uuid
        );
    }

    #[test]
    fn infer_text_for_unknown() {
        assert_eq!(
            infer_result_kind_from_column(Some(&col("c", "nvarchar(max)"))),
            ResultKind::Text
        );
        assert_eq!(infer_result_kind_from_column(None), ResultKind::Text);
    }

    #[test]
    fn infer_from_value_when_column_is_text() {
        assert_eq!(
            infer_result_kind(None, &CellValue::I64(42), Some(&col("c", "varchar"))),
            ResultKind::Integer
        );
        assert_eq!(
            infer_result_kind(None, &CellValue::F64(1.5), Some(&col("c", "varchar"))),
            ResultKind::Decimal
        );
        assert_eq!(
            infer_result_kind(None, &CellValue::Bool(true), Some(&col("c", "varchar"))),
            ResultKind::Bool
        );
    }

    #[test]
    fn infer_column_kind_wins_over_value() {
        // A string "42" in an int column should be Integer, not sniffed
        assert_eq!(
            infer_result_kind(
                None,
                &CellValue::String("42".into()),
                Some(&col("c", "int"))
            ),
            ResultKind::Integer
        );
    }

    #[test]
    fn sniff_uuid_with_dashes() {
        assert_eq!(
            infer_result_kind(
                None,
                &CellValue::String("550e8400-e29b-41d4-a716-446655440000".into()),
                Some(&col("c", "nvarchar"))
            ),
            ResultKind::Uuid
        );
    }

    #[test]
    fn sniff_uuid_without_dashes() {
        assert_eq!(
            infer_result_kind(
                None,
                &CellValue::String("550e8400e29b41d4a716446655440000".into()),
                Some(&col("c", "nvarchar"))
            ),
            ResultKind::Uuid
        );
    }

    #[test]
    fn sniff_date_string() {
        assert_eq!(
            infer_result_kind(
                None,
                &CellValue::String("2024-01-15".into()),
                Some(&col("c", "varchar"))
            ),
            ResultKind::Date
        );
    }

    #[test]
    fn sniff_plain_text_stays_text() {
        assert_eq!(
            infer_result_kind(
                None,
                &CellValue::String("hello world".into()),
                Some(&col("c", "varchar"))
            ),
            ResultKind::Text
        );
    }

    #[test]
    fn temporal_kind_datetime() {
        assert_eq!(
            temporal_kind(Some(&col("c", "datetime"))),
            ResultTemporalKind::DateTime
        );
        assert_eq!(
            temporal_kind(Some(&col("c", "datetime2"))),
            ResultTemporalKind::DateTime
        );
    }

    #[test]
    fn temporal_kind_date_only() {
        assert_eq!(
            temporal_kind(Some(&col("c", "date"))),
            ResultTemporalKind::Date
        );
    }

    #[test]
    fn temporal_kind_time_only() {
        assert_eq!(
            temporal_kind(Some(&col("c", "time"))),
            ResultTemporalKind::Time
        );
    }

    #[test]
    fn temporal_kind_unknown_for_non_temporal() {
        assert_eq!(
            temporal_kind(Some(&col("c", "int"))),
            ResultTemporalKind::Unknown
        );
        assert_eq!(temporal_kind(None), ResultTemporalKind::Unknown);
    }

    #[test]
    fn resolve_uses_inferred_kind_by_default() {
        let prefs = ResultFormattingPreferences::default();
        let resolved = resolve(&prefs, "col1", ResultKind::Integer);
        assert_eq!(resolved.kind, ResultKind::Integer);
        assert_eq!(resolved.alignment(), ResultTextAlignment::Right);
    }

    #[test]
    fn resolve_override_wins_when_not_inherit() {
        let mut prefs = ResultFormattingPreferences::default();
        let override_fmt = ColumnFormat {
            kind: ResultKind::Text, // "inherit"
            ..Default::default()
        };
        prefs
            .column_overrides
            .insert("col1".to_string(), override_fmt);
        let resolved = resolve(&prefs, "col1", ResultKind::Integer);
        // Override is "inherit" so inferred kind wins
        assert_eq!(resolved.kind, ResultKind::Integer);
    }

    #[test]
    fn resolve_override_kind_wins_when_explicit() {
        let mut prefs = ResultFormattingPreferences::default();
        let override_fmt = ColumnFormat {
            kind: ResultKind::Decimal,
            ..Default::default()
        };
        prefs
            .column_overrides
            .insert("col1".to_string(), override_fmt);
        let resolved = resolve(&prefs, "col1", ResultKind::Integer);
        assert_eq!(resolved.kind, ResultKind::Decimal);
    }

    #[test]
    fn column_alignment_right_for_integer() {
        let prefs = ResultFormattingPreferences::default();
        let metadata = vec![col("count", "int")];
        assert_eq!(
            column_alignment(None, 0, &metadata, &prefs, None),
            ResultTextAlignment::Right
        );
    }

    #[test]
    fn column_alignment_left_for_text() {
        let prefs = ResultFormattingPreferences::default();
        let metadata = vec![col("name", "varchar")];
        assert_eq!(
            column_alignment(None, 0, &metadata, &prefs, None),
            ResultTextAlignment::Left
        );
    }

    #[test]
    fn is_likely_uuid_valid() {
        assert!(is_likely_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_likely_uuid("550e8400e29b41d4a716446655440000"));
    }

    #[test]
    fn is_likely_uuid_rejects_short() {
        assert!(!is_likely_uuid("550e8400"));
        assert!(!is_likely_uuid("not-a-uuid"));
    }

    #[test]
    fn is_likely_date_valid() {
        assert!(is_likely_date("2024-01-15"));
        assert!(is_likely_date("01/15/2024"));
    }

    #[test]
    fn is_likely_date_rejects_plain_text() {
        assert!(!is_likely_date("hello"));
        assert!(!is_likely_date("12345"));
    }

    #[test]
    fn mask_text_table() {
        let cases: Vec<(&str, &str, usize, usize, &str)> = vec![
            ("card last 4", "4111111111111111", 0, 4, "••••••••••••1111"),
            ("prefix+suffix cover all", "abcd", 2, 2, "abcd"),
            ("multibyte prefix", "héllo wörld", 6, 0, "héllo •••••"),
            ("empty", "", 0, 4, ""),
        ];
        for (case, raw, prefix, suffix, expected) in cases {
            let fmt = MaskFormat {
                keep_prefix: prefix,
                keep_suffix: suffix,
                mask_char: '•',
            };
            assert_eq!(mask_text(raw, &fmt), expected, "{case}");
        }
    }

    #[test]
    fn epoch_to_datetime_table() {
        let utc = EpochFormat {
            unit: EpochUnit::Auto,
            pattern: "%Y-%m-%d %H:%M:%S".into(),
            ..Default::default()
        };
        let seconds = EpochFormat {
            unit: EpochUnit::Seconds,
            ..utc.clone()
        };
        let cases: Vec<(&str, CellValue, &EpochFormat, Option<&str>)> = vec![
            (
                "seconds",
                CellValue::I64(1_431_648_000),
                &utc,
                Some("2015-05-15 00:00:00"),
            ),
            (
                "auto millis",
                CellValue::I64(1_431_648_000_000),
                &utc,
                Some("2015-05-15 00:00:00"),
            ),
            (
                "seconds unit on millis is out of range",
                CellValue::I64(1_431_648_000_000),
                &seconds,
                None,
            ),
            (
                "negative epoch",
                CellValue::I64(-1),
                &utc,
                Some("1969-12-31 23:59:59"),
            ),
            (
                "numeric string",
                CellValue::String("1431648000".into()),
                &utc,
                Some("2015-05-15 00:00:00"),
            ),
            (
                "non-numeric string",
                CellValue::String("abc".into()),
                &utc,
                None,
            ),
            ("bool", CellValue::Bool(true), &utc, None),
        ];
        for (case, value, fmt, expected) in cases {
            assert_eq!(
                epoch_to_datetime(&value, fmt).as_deref(),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn named_zone_shifts_hours() {
        let zone = resolve_zone(DateTimeZoneMode::AlwaysInTimeZone, "Asia/Tokyo").unwrap();
        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            render_in_zone(utc, &zone, "%Y-%m-%d %H:%M"),
            "2026-01-01 21:00"
        );
    }

    #[test]
    fn invalid_identifier_errors() {
        let err = resolve_zone(DateTimeZoneMode::AlwaysInTimeZone, "Mars/Olympus").unwrap_err();
        assert_eq!(err, UnknownTimeZone("Mars/Olympus".into()));
        assert!(err.message().contains("Mars/Olympus"), "{err:?}");
        assert!(resolve_zone(DateTimeZoneMode::AlwaysInTimeZone, "  ").is_err());
        // Other modes never look at the identifier.
        assert_eq!(
            resolve_zone(DateTimeZoneMode::AlwaysUtc, "Mars/Olympus"),
            Ok(ResolvedZone::Utc)
        );
    }

    #[test]
    fn as_stored_leaves_naive_unchanged() {
        let fmt = DateFormat {
            time_zone_mode: DateTimeZoneMode::NativeAsStored,
            ..Default::default()
        };
        assert_eq!(format_datetime_text("2026-01-01 12:00:00", &fmt), None);
        // Offset-carrying text also stays exactly as the engine sent it.
        assert_eq!(
            format_datetime_text("2026-01-01T12:00:00+02:00", &fmt),
            None
        );
    }

    #[test]
    fn timestamptz_renders_in_named_zone() {
        let fmt = DateFormat {
            time_zone_mode: DateTimeZoneMode::AlwaysInTimeZone,
            time_zone_identifier: "Europe/Berlin".into(),
            ..Default::default()
        };
        // Winter: Berlin is UTC+1.
        assert_eq!(
            format_datetime_text("2026-01-01T12:00:00+00:00", &fmt).as_deref(),
            Some("2026-01-01 13:00:00")
        );
        // Naive values count as UTC and still shift.
        assert_eq!(
            format_datetime_text("2026-01-01 12:00:00", &fmt).as_deref(),
            Some("2026-01-01 13:00:00")
        );
        // Fractional seconds survive the re-render.
        assert_eq!(
            format_datetime_text("2026-01-01 12:00:00.500", &fmt).as_deref(),
            Some("2026-01-01 13:00:00.500")
        );
        // Date-only and unparseable text stay untouched.
        assert_eq!(format_datetime_text("2026-01-01", &fmt), None);
        assert_eq!(format_datetime_text("not a date", &fmt), None);
    }

    #[test]
    fn epoch_renders_in_named_zone_and_falls_back_to_utc_on_bad_id() {
        let named = EpochFormat {
            unit: EpochUnit::Seconds,
            pattern: "%Y-%m-%d %H:%M:%S".into(),
            time_zone_mode: Some(DateTimeZoneMode::AlwaysInTimeZone),
            time_zone_identifier: "Asia/Tokyo".into(),
        };
        // 2026-01-01T12:00:00Z → 21:00 in Tokyo.
        assert_eq!(
            epoch_to_datetime(&CellValue::I64(1_767_268_800), &named).as_deref(),
            Some("2026-01-01 21:00:00")
        );
        let bad = EpochFormat {
            time_zone_identifier: "Nowhere/Здесь".into(),
            ..named
        };
        assert_eq!(
            epoch_to_datetime(&CellValue::I64(1_767_268_800), &bad).as_deref(),
            Some("2026-01-01 12:00:00")
        );
    }

    #[test]
    fn explicit_epoch_utc_choice_survives_a_reload() {
        use crate::preferences::ColumnZoneChoice;
        let mut fmt = EpochFormat::default();
        assert_eq!(fmt.zone_choice(), ColumnZoneChoice::FollowGlobal);
        fmt.set_zone_choice(ColumnZoneChoice::Utc);
        let json = serde_json::to_string(&fmt).unwrap();
        let back: EpochFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.zone_choice(), ColumnZoneChoice::Utc, "{json}");
        fmt.set_zone_choice(ColumnZoneChoice::FollowGlobal);
        let back: EpochFormat =
            serde_json::from_str(&serde_json::to_string(&fmt).unwrap()).unwrap();
        assert_eq!(back.zone_choice(), ColumnZoneChoice::FollowGlobal);
        // "As stored" is meaningless for an instant: it pins UTC.
        fmt.set_zone_choice(ColumnZoneChoice::AsStored);
        assert_eq!(fmt.zone_choice(), ColumnZoneChoice::Utc);
    }

    #[test]
    fn date_overrides_follow_the_global_zone_until_pinned() {
        use crate::preferences::ColumnZoneChoice;
        let defaults = DateFormat {
            time_zone_mode: DateTimeZoneMode::AlwaysInTimeZone,
            time_zone_identifier: "Asia/Tokyo".into(),
            ..Default::default()
        };
        // A brand-new Date override, and one saved before zones existed.
        let new_override = ColumnFormat::default_for_kind(ResultKind::Date);
        let legacy: ColumnFormat = serde_json::from_str(
            r#"{"kind":"date","dateFormat":{"format":"yyyy-MM-dd","timeZoneMode":"alwaysUtc","timeZoneIdentifier":"UTC","alignment":"left"}}"#,
        )
        .unwrap();
        for fmt in [&new_override, &legacy] {
            assert_eq!(
                fmt.date_format.zone_choice(),
                ColumnZoneChoice::FollowGlobal
            );
            let effective = effective_override_date_format(Some(fmt), &defaults);
            assert_eq!(effective.time_zone_identifier, "Asia/Tokyo");
            assert_eq!(effective.time_zone_mode, DateTimeZoneMode::AlwaysInTimeZone);
        }
        assert_eq!(
            effective_override_date_format(Some(&legacy), &defaults).format,
            "yyyy-MM-dd",
            "the override's own pattern is kept"
        );
        // An explicit UTC choice is pinned and survives a save/load.
        let mut pinned = new_override.clone();
        pinned.date_format.set_zone_choice(ColumnZoneChoice::Utc);
        let pinned: ColumnFormat =
            serde_json::from_str(&serde_json::to_string(&pinned).unwrap()).unwrap();
        assert_eq!(pinned.date_format.zone_choice(), ColumnZoneChoice::Utc);
        let effective = effective_override_date_format(Some(&pinned), &defaults);
        assert_eq!(effective.time_zone_mode, DateTimeZoneMode::AlwaysUtc);
        // Back to following: the stored file stays as small as before.
        let mut back = pinned.clone();
        back.date_format
            .set_zone_choice(ColumnZoneChoice::FollowGlobal);
        assert!(!serde_json::to_string(&back).unwrap().contains("zonePinned"));
        // Every other choice is pinned.
        for choice in [
            ColumnZoneChoice::Local,
            ColumnZoneChoice::AsStored,
            ColumnZoneChoice::Named,
        ] {
            let mut fmt = DateFormat::default();
            fmt.set_zone_choice(choice);
            assert_eq!(fmt.zone_choice(), choice);
        }
    }

    #[test]
    fn connection_zone_replaces_only_the_global_default() {
        use sqlly_core::DisplayTimeZone;
        let mut prefs = ResultFormattingPreferences::default();
        apply_connection_display_zone(&mut prefs, None);
        assert_eq!(prefs, ResultFormattingPreferences::default());
        apply_connection_display_zone(
            &mut prefs,
            Some(&DisplayTimeZone::Named("Europe/Berlin".into())),
        );
        assert_eq!(
            prefs.defaults.date_format.time_zone_mode,
            DateTimeZoneMode::AlwaysInTimeZone
        );
        assert_eq!(
            prefs.defaults.date_format.time_zone_identifier,
            "Europe/Berlin"
        );
        apply_connection_display_zone(&mut prefs, Some(&DisplayTimeZone::Local));
        assert_eq!(
            prefs.defaults.date_format.time_zone_mode,
            DateTimeZoneMode::AlwaysLocal
        );
        apply_connection_display_zone(&mut prefs, Some(&DisplayTimeZone::Utc));
        assert_eq!(
            prefs.defaults.date_format.time_zone_mode,
            DateTimeZoneMode::AlwaysUtc
        );
    }

    #[test]
    fn epoch_column_names_are_recognised() {
        for name in [
            "created_at",
            "UPDATED_AT",
            "updatedAt",
            "event_ts",
            "login_time",
            "LastLoginTimestamp",
            "signup_epoch",
            "timestamp",
            "ts",
            "mtime",
            "created",
            "expires-at",
        ] {
            assert!(epoch_name_candidate(name), "{name}");
        }
        for name in [
            "id",
            "amount",
            "response_time",
            "elapsed_time",
            "total_ts",
            "turned_on",
            "birth_date",
            "status",
            "seat",
            "format",
            "",
        ] {
            assert!(!epoch_name_candidate(name), "{name}");
        }
    }

    #[test]
    fn plausible_epoch_range_covers_seconds_and_millis() {
        assert!(plausible_epoch(1_767_268_800)); // 2026 in seconds
        assert!(plausible_epoch(1_767_268_800_000)); // 2026 in millis
        assert!(!plausible_epoch(3600));
        assert!(!plausible_epoch(0));
        assert!(!plausible_epoch(20_260_101)); // a yyyymmdd integer
        assert!(!plausible_epoch(946_684_799)); // 1999-12-31
        assert!(!plausible_epoch(4_102_444_800)); // 2100-01-01
        assert!(plausible_epoch_cell(&CellValue::String(
            " 1767268800 ".into()
        )));
        assert!(!plausible_epoch_cell(&CellValue::Null));
    }

    #[test]
    fn epoch_utc_alias_migrates() {
        // Pre-268 stored preferences carried `utc: bool`; `false` maps onto
        // AlwaysLocal. A stored UTC (legacy or `alwaysUtc`) was only ever the
        // saved default, so it loads as "follow the global zone"; any other
        // explicit `timeZoneMode` wins.
        let legacy_utc: EpochFormat = serde_json::from_str(r#"{"utc": true}"#).unwrap();
        assert_eq!(legacy_utc.time_zone_mode, None);
        let legacy_local: EpochFormat =
            serde_json::from_str(r#"{"unit":"seconds","utc": false}"#).unwrap();
        assert_eq!(
            legacy_local.time_zone_mode,
            Some(DateTimeZoneMode::AlwaysLocal)
        );
        assert_eq!(legacy_local.unit, EpochUnit::Seconds);
        let saved_default: EpochFormat =
            serde_json::from_str(r#"{"utc": false, "timeZoneMode":"alwaysUtc"}"#).unwrap();
        assert_eq!(saved_default.time_zone_mode, None);
        let explicit: EpochFormat = serde_json::from_str(
            r#"{"timeZoneMode":"alwaysInTimeZone","timeZoneIdentifier":"Asia/Tokyo"}"#,
        )
        .unwrap();
        assert_eq!(
            explicit.time_zone_mode,
            Some(DateTimeZoneMode::AlwaysInTimeZone)
        );
        // Round-trip: the new shape serializes without `utc` and reloads.
        let json = serde_json::to_string(&legacy_local).unwrap();
        assert!(!json.contains("\"utc\""));
        let reloaded: EpochFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, legacy_local);
    }

    #[test]
    fn display_transform_null_mask_is_none() {
        let fmt = ColumnFormat {
            kind: ResultKind::Mask,
            ..Default::default()
        };
        assert_eq!(display_transform(&CellValue::Null, &fmt), None);
    }

    #[test]
    fn apply_export_masks_honours_flag() {
        let columns = [col("secret", "nvarchar")];
        let rows = vec![vec![CellValue::String("4111111111111111".into())]];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "secret".into(),
            ColumnFormat {
                kind: ResultKind::Mask,
                ..Default::default()
            },
        );
        let masked = apply_export_masks(&columns, rows.clone(), &prefs);
        assert_eq!(masked[0][0], CellValue::String("••••••••••••1111".into()));
        prefs.exports_masked = false;
        let raw = apply_export_masks(&columns, rows, &prefs);
        assert_eq!(raw[0][0], CellValue::String("4111111111111111".into()));
    }

    #[test]
    fn display_transform_json_path_extracts_and_keeps_raw_on_failure() {
        let fmt = ColumnFormat {
            kind: ResultKind::JsonPath,
            json_path_format: crate::preferences::JsonPathFormat {
                path: "$.customer.name".into(),
                missing_text: "(missing)".into(),
            },
            ..Default::default()
        };
        let extracted = display_transform(
            &CellValue::String(r#"{"customer":{"name":"Ada"}}"#.into()),
            &fmt,
        );
        assert_eq!(extracted, Some(CellValue::String("Ada".into())));
        let missing = display_transform(&CellValue::String("not-json".into()), &fmt);
        assert_eq!(missing, Some(CellValue::String("(missing)".into())));
    }

    #[test]
    fn display_transform_template_skips_null() {
        let fmt = ColumnFormat {
            kind: ResultKind::Template,
            template_format: crate::preferences::TemplateFormat {
                template: "{value|upper}".into(),
            },
            ..Default::default()
        };
        assert_eq!(display_transform(&CellValue::Null, &fmt), None);
        assert_eq!(
            display_transform(&CellValue::String("ada".into()), &fmt),
            Some(CellValue::String("ADA".into()))
        );
    }

    #[test]
    fn display_transform_number_percent_from_decimal_string() {
        let fmt = ColumnFormat {
            kind: ResultKind::Number,
            number_style_format: crate::preferences::NumberStyleFormat {
                style: crate::preferences::NumberStyle::Percent,
                decimals: 2,
                use_thousands_separators: true,
            },
            ..Default::default()
        };
        assert_eq!(
            display_transform(&CellValue::String("0.25".into()), &fmt),
            Some(CellValue::String("25.00%".into()))
        );
    }

    #[test]
    fn apply_export_masks_ignores_json_path_override() {
        let columns = [col("payload", "nvarchar")];
        let rows = vec![vec![CellValue::String(r#"{"a":1}"#.into())]];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "payload".into(),
            ColumnFormat {
                kind: ResultKind::JsonPath,
                json_path_format: crate::preferences::JsonPathFormat {
                    path: "$.a".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let exported = apply_export_masks(&columns, rows, &prefs);
        assert_eq!(exported[0][0], CellValue::String(r#"{"a":1}"#.into()));
    }

    #[test]
    fn mask_directive_masks_copies_and_exports() {
        let columns = [col("Card", "nvarchar"), col("Name", "nvarchar")];
        let rows = vec![vec![
            CellValue::String("4111111111111111".into()),
            CellValue::String("Ada".into()),
        ]];
        let hints = crate::query_format::parse("-- sqlly format: column=card; mask=2/4\nSELECT 1");
        let mut prefs = ResultFormattingPreferences::default();
        inject_directive_masks(&mut prefs, &columns, &hints);
        let exported = apply_export_masks(&columns, rows.clone(), &prefs);
        assert_eq!(exported[0][0], CellValue::String("41••••••••••1111".into()));
        assert_eq!(exported[0][1], CellValue::String("Ada".into()));
        // "Mask exports" off exports the raw value, as for override masks.
        prefs.exports_masked = false;
        let raw = apply_export_masks(&columns, rows, &prefs);
        assert_eq!(raw[0][0], CellValue::String("4111111111111111".into()));
    }

    #[test]
    fn mask_directive_wins_over_a_configured_override() {
        let columns = [col("Card", "nvarchar")];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "Card".into(),
            ColumnFormat::default_for_kind(ResultKind::Uuid),
        );
        let hints = crate::query_format::parse("-- sqlly format: column=Card; mask=0/2\nSELECT 1");
        inject_directive_masks(&mut prefs, &columns, &hints);
        let rows = vec![vec![CellValue::String("123456".into())]];
        let exported = apply_export_masks(&columns, rows, &prefs);
        assert_eq!(exported[0][0], CellValue::String("••••56".into()));
        // Non-mask directives leave the overrides alone.
        let mut plain = ResultFormattingPreferences::default();
        let hints = crate::query_format::parse("-- sqlly format: column=Card; epoch=s\nSELECT 1");
        inject_directive_masks(&mut plain, &columns, &hints);
        assert!(plain.column_overrides.is_empty());
    }

    fn source(table: &[&str], column: &str) -> ColumnSource {
        ColumnSource {
            table: table.iter().map(|part| (*part).to_string()).collect(),
            column: column.into(),
        }
    }

    #[test]
    fn qualified_key_matching() {
        let column = col("CustomerName", "nvarchar");
        let src = source(&["dbo", "Customers"], "Name");
        assert!(qualified_key_matches("customers.name", &column, &src));
        assert!(qualified_key_matches("dbo.Customers.Name", &column, &src));
        assert!(qualified_key_matches(
            "[dbo].[Customers].[Name]",
            &column,
            &src
        ));
        // The result column's own name works after the table too.
        assert!(qualified_key_matches(
            "Customers.CustomerName",
            &column,
            &src
        ));
        assert!(!qualified_key_matches("Orders.Name", &column, &src));
        assert!(!qualified_key_matches(
            "sales.Customers.Name",
            &column,
            &src
        ));
        assert!(!qualified_key_matches(
            "x.dbo.Customers.Name",
            &column,
            &src
        ));
        // Bare keys are matched by name elsewhere, never here.
        assert!(!qualified_key_matches("Name", &column, &src));
    }

    #[test]
    fn qualified_override_wins_over_bare_name_and_needs_a_known_source() {
        let columns = [col("Total", "int"), col("Other", "int")];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "Total".into(),
            ColumnFormat::default_for_kind(ResultKind::Decimal),
        );
        prefs.column_overrides.insert(
            "orders.total".into(),
            ColumnFormat::default_for_kind(ResultKind::Mask),
        );
        // Unknown source: the bare-name override stays.
        let mut unknown = prefs.clone();
        resolve_qualified_overrides(&mut unknown, &columns, &[None, None]);
        assert_eq!(unknown.column_overrides["Total"].kind, ResultKind::Decimal);
        // Known source: the qualified override wins.
        resolve_qualified_overrides(
            &mut prefs,
            &columns,
            &[Some(source(&["Orders"], "Total")), None],
        );
        assert_eq!(prefs.column_overrides["Total"].kind, ResultKind::Mask);
        assert!(!prefs.column_overrides.contains_key("Other"));
    }

    #[test]
    fn qualified_override_applies_without_a_bare_name_override() {
        let columns = [col("Name", "nvarchar")];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "customers.name".into(),
            ColumnFormat::default_for_kind(ResultKind::Mask),
        );
        resolve_qualified_overrides(
            &mut prefs,
            &columns,
            &[Some(source(&["Sales", "Customers"], "Name"))],
        );
        let rows = vec![vec![CellValue::String("Ada Lovelace".into())]];
        let exported = apply_export_masks(&columns, rows, &prefs);
        assert_eq!(exported[0][0], CellValue::String("••••••••lace".into()));
    }

    #[test]
    fn same_named_columns_from_different_tables_keep_the_bare_override() {
        // `SELECT o.Id, c.Id …`: both result columns are named `Id`, and
        // downstream lookups are by name, so a qualified key matching only
        // one of them cannot be filed under `Id` without hitting both.
        let columns = [col("Id", "int"), col("Id", "int")];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "orders.id".into(),
            ColumnFormat::default_for_kind(ResultKind::Mask),
        );
        resolve_qualified_overrides(
            &mut prefs,
            &columns,
            &[
                Some(source(&["Orders"], "Id")),
                Some(source(&["Customers"], "Id")),
            ],
        );
        assert!(!prefs.column_overrides.contains_key("Id"));
        // When every same-named column agrees, it applies.
        resolve_qualified_overrides(
            &mut prefs,
            &columns,
            &[
                Some(source(&["Orders"], "Id")),
                Some(source(&["Orders"], "Id")),
            ],
        );
        assert_eq!(prefs.column_overrides["Id"].kind, ResultKind::Mask);
    }
}
