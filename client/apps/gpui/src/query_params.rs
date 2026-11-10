//! Query parameter directives: `-- sqlly param: @name <type> [= default]`.
//!
//! Declared parameters surface as a strip of typed inputs; Run substitutes
//! each `@name` occurrence (outside strings and comments) with a validated,
//! dialect-correct literal before the SQL leaves the app. Pure — the parsing,
//! validation, and substitution carry no GPUI types.

use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

/// The value families a parameter can declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Text,
    Int,
    Decimal,
    Bool,
    Date,
    DateTime,
    Time,
    Uuid,
    Json,
    Binary,
}

impl ParamType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::DateTime => "datetime",
            Self::Time => "time",
            Self::Uuid => "uuid",
            Self::Json => "json",
            Self::Binary => "binary",
        }
    }

    pub(crate) fn parse(token: &str) -> Result<Self, String> {
        Ok(match token.trim().to_ascii_lowercase().as_str() {
            "text" | "string" | "varchar" | "nvarchar" => Self::Text,
            "int" | "integer" | "bigint" => Self::Int,
            "decimal" | "number" | "numeric" | "float" | "money" => Self::Decimal,
            "bool" | "boolean" | "bit" => Self::Bool,
            "date" => Self::Date,
            "datetime" | "timestamp" => Self::DateTime,
            "time" => Self::Time,
            "uuid" | "guid" | "uniqueidentifier" => Self::Uuid,
            "json" | "jsonb" => Self::Json,
            "binary" | "varbinary" | "bytes" | "blob" => Self::Binary,
            other => {
                return Err(format!(
                    "unknown type '{other}' — use text, int, decimal, bool, date, datetime, \
                     time, uuid, json, or binary"
                ));
            }
        })
    }
}

/// One declared parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    /// Includes the leading `@`.
    pub name: String,
    pub ty: ParamType,
    pub default: Option<String>,
}

/// Parse one directive body (the text after `param:`).
pub fn parse_spec(body: &str) -> Result<ParamSpec, String> {
    let body = body.trim();
    let rest = body
        .strip_prefix('@')
        .ok_or_else(|| crate::localization::text("query.params.name_requires_at"))?;
    let name_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count();
    if name_len == 0 {
        return Err(crate::localization::text("query.params.name_requires_at"));
    }
    let name = format!("@{}", &rest[..name_len]);
    let rest = rest[name_len..].trim();
    if rest.is_empty() {
        return Err(crate::localization::text("query.params.type_required"));
    }
    let (type_token, default) = match rest.split_once('=') {
        Some((ty, default)) => (ty.trim(), Some(default.trim().to_string())),
        None => (rest, None),
    };
    let ty = ParamType::parse(type_token)?;
    if let Some(default) = default.as_deref()
        && let Err(err) = validate_value(ty, default)
    {
        return Err(crate::localization::format(
            "query.params.invalid_default",
            &[("error", &err)],
        ));
    }
    Ok(ParamSpec {
        name,
        ty,
        default: default.filter(|d| !d.is_empty()),
    })
}

/// Every declared parameter in the buffer, in order, first declaration wins
/// on duplicate names.
pub fn parse_param_directives(sql: &str) -> Vec<ParamSpec> {
    let mut specs: Vec<ParamSpec> = Vec::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("--") {
            continue;
        }
        if let Some(crate::sqlly_directive::Directive::Param { body }) =
            crate::sqlly_directive::classify(trimmed)
            && let Ok(spec) = parse_spec(body)
            && !specs
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&spec.name))
        {
            specs.push(spec);
        }
    }
    specs
}

/// One `-- sqlly set` variable: `name` without a sigil, `value` verbatim text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableSpec {
    pub name: String,
    pub value: String,
}

/// Parse `<name> = <value>`: name is `[A-Za-z_][A-Za-z0-9_]*`, `=` required,
/// value is the rest trimmed (may be empty → error "a value is required").
/// Reject a name that starts with `@`, `$`, `:` — variables have no sigil;
/// the placeholder written in SQL is `${name}`.
pub fn parse_set_spec(body: &str) -> Result<VariableSpec, String> {
    let body = body.trim();
    let Some((name_part, value_part)) = body.split_once('=') else {
        return Err(crate::localization::text("query.params.equals_required"));
    };
    let name = name_part.trim();
    if name.is_empty() {
        return Err(crate::localization::text(
            "query.params.variable_name_required",
        ));
    }
    if name.starts_with('@') || name.starts_with('$') || name.starts_with(':') {
        return Err(crate::localization::text(
            "query.params.variable_sigil_forbidden",
        ));
    }
    let first = name.chars().next().unwrap_or(' ');
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(crate::localization::text(
            "query.params.variable_start_invalid",
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(crate::localization::text(
            "query.params.variable_characters_invalid",
        ));
    }
    let value = value_part.trim();
    if value.is_empty() {
        return Err(crate::localization::text("query.params.value_required"));
    }
    Ok(VariableSpec {
        name: name.to_string(),
        value: value.to_string(),
    })
}

/// Every `-- sqlly set` line in order; last assignment wins.
pub fn parse_set_directives(sql: &str) -> Vec<VariableSpec> {
    let mut vars: Vec<VariableSpec> = Vec::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("--") {
            continue;
        }
        if let Some(crate::sqlly_directive::Directive::Set { body }) =
            crate::sqlly_directive::classify(trimmed)
            && let Ok(spec) = parse_set_spec(body)
        {
            if let Some(existing) = vars
                .iter_mut()
                .find(|v| v.name.eq_ignore_ascii_case(&spec.name))
            {
                *existing = spec;
            } else {
                vars.push(spec);
            }
        }
    }
    vars
}

/// Replace every `${name}` occurrence outside strings, comments, and quoted
/// identifiers with the verbatim value. Unknown `${...}` tokens are left as-is.
pub fn substitute_variables(db: DatabaseType, sql: &str, vars: &[VariableSpec]) -> String {
    if vars.is_empty() {
        return sql.to_string();
    }
    rewrite_code_tokens(db, sql, |remaining, out| {
        if !remaining.starts_with("${") {
            return None;
        }
        let rest = &remaining[2..];
        let name_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if name_len == 0 {
            return None;
        }
        let after_name = &rest[name_len..];
        if !after_name.starts_with('}') {
            return None;
        }
        let token = &remaining[2..2 + name_len];
        if let Some(spec) = vars
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(token))
        {
            out.push_str(&spec.value);
            Some(2 + name_len + 1)
        } else {
            None
        }
    })
}

/// Validate a raw strip value against its declared type. `NULL` (any case)
/// is always accepted and substitutes as SQL NULL.
pub fn validate_value(ty: ParamType, raw: &str) -> Result<(), String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("null") {
        return Ok(());
    }
    let ok = match ty {
        ParamType::Text => true,
        ParamType::Int => raw.parse::<i64>().is_ok(),
        ParamType::Decimal => {
            raw.parse::<f64>().is_ok_and(f64::is_finite)
                && raw
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
        }
        ParamType::Bool => matches!(
            raw.to_ascii_lowercase().as_str(),
            "true" | "false" | "0" | "1" | "yes" | "no"
        ),
        ParamType::Date => is_iso_date(raw),
        ParamType::DateTime => is_iso_date(raw) || is_iso_datetime_like(raw),
        ParamType::Time => is_iso_time(raw),
        ParamType::Uuid => is_uuid(raw),
        ParamType::Json => serde_json::from_str::<serde_json::Value>(raw).is_ok(),
        ParamType::Binary => parse_hex_bytes(raw).is_some(),
    };
    if ok {
        Ok(())
    } else {
        Err(crate::localization::format(
            "query.params.value_type_invalid",
            &[("parameter_type", ty.label())],
        ))
    }
}

/// The first declared parameter whose effective value (typed value, else
/// declared default) is missing or fails type validation — the row Run
/// should focus when substitution blocks. `None` when every parameter
/// resolves.
pub fn first_unresolved(specs: &[ParamSpec], values: &[(String, String)]) -> Option<String> {
    for spec in specs {
        let typed = values
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&spec.name))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty());
        let effective = typed.or(spec.default.as_deref());
        match effective {
            None => return Some(spec.name.clone()),
            Some(value) => {
                if validate_value(spec.ty, value).is_err() {
                    return Some(spec.name.clone());
                }
            }
        }
    }
    None
}

fn is_iso_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
}

fn is_iso_datetime_like(value: &str) -> bool {
    match value.find(['T', ' ']) {
        Some(split) => {
            let (date, time) = value.split_at(split);
            is_iso_date(date)
                && time[1..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'.' | b'Z'))
                && time.len() > 1
        }
        None => false,
    }
}

/// The literal a validated value renders as.
fn literal(db: DatabaseType, ty: ParamType, raw: &str) -> String {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    match ty {
        // A negative number is parenthesized: spliced after a `-` in the
        // user's SQL (`10 -@n`), a bare `-5` would form `--` and comment out
        // the rest of the line.
        ParamType::Int | ParamType::Decimal if raw.starts_with('-') => format!("({raw})"),
        ParamType::Int | ParamType::Decimal => raw.to_string(),
        ParamType::Bool => db
            .bool_literal(matches!(
                raw.to_ascii_lowercase().as_str(),
                "true" | "1" | "yes"
            ))
            .to_string(),
        ParamType::Text
        | ParamType::Date
        | ParamType::DateTime
        | ParamType::Time
        | ParamType::Uuid
        | ParamType::Json => db.string_literal(raw),
        // A validated hex string renders as the engine's binary literal
        // (`0xDEAD`, `X'DEAD'`, `'\xDEAD'`, …) via the shared dialect helper.
        ParamType::Binary => match parse_hex_bytes(raw) {
            Some(bytes) => db.bytes_literal(&bytes),
            // Unreachable for a validated value; fall back to a string literal.
            None => db.string_literal(raw),
        },
    }
}

/// `HH:MM`, `HH:MM:SS`, or `HH:MM:SS.fff` with in-range fields.
fn is_iso_time(value: &str) -> bool {
    let (clock, frac) = match value.split_once('.') {
        Some((clock, frac)) => (clock, Some(frac)),
        None => (value, None),
    };
    if let Some(frac) = frac
        && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let parts: Vec<&str> = clock.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) {
        return false;
    }
    let bounds = [24u32, 60, 60];
    parts.iter().enumerate().all(|(i, part)| {
        part.len() == 2
            && part.bytes().all(|b| b.is_ascii_digit())
            && part.parse::<u32>().is_ok_and(|n| n < bounds[i])
    })
}

/// A UUID with dashes (`8-4-4-4-12`) or 32 bare hex digits, case-insensitive.
fn is_uuid(value: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    if value.len() == 36 {
        let groups: Vec<&str> = value.split('-').collect();
        matches!(groups.as_slice(), [a, b, c, d, e]
            if a.len() == 8 && b.len() == 4 && c.len() == 4 && d.len() == 4 && e.len() == 12
            && [a, b, c, d, e].iter().all(|g| hex(g)))
    } else {
        value.len() == 32 && hex(value)
    }
}

/// Decode an even-length hex string (optionally `0x`-prefixed) into bytes,
/// or `None` if it is not valid hex.
fn parse_hex_bytes(value: &str) -> Option<Vec<u8>> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Substitute every declared parameter in `sql` with its value's literal.
/// Values come from the strip; an empty value falls back to the declared
/// default, and a parameter with neither blocks the run with a readable
/// error. Replacement is string/comment-aware — an `@name` inside a quoted
/// literal or a comment is left alone — and only whole names match (`@id`
/// never rewrites part of `@idx`).
pub fn substitute_params(
    db: DatabaseType,
    sql: &str,
    specs: &[ParamSpec],
    values: &[(String, String)],
) -> Result<String, String> {
    if specs.is_empty() {
        return Ok(sql.to_string());
    }
    // Resolve each spec to its literal first, so every problem is reported
    // before any text is rewritten.
    let mut resolved: Vec<(&ParamSpec, String)> = Vec::with_capacity(specs.len());
    let mut missing: Vec<&str> = Vec::new();
    for spec in specs {
        let supplied = values
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&spec.name))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty());
        let raw = supplied.or(spec.default.as_deref());
        match raw {
            Some(raw) => {
                validate_value(spec.ty, raw).map_err(|err| {
                    crate::localization::format(
                        "query.params.named_value_invalid",
                        &[("name", &spec.name), ("error", &err)],
                    )
                })?;
                resolved.push((spec, literal(db, spec.ty, raw)));
            }
            None => missing.push(&spec.name),
        }
    }
    if !missing.is_empty() {
        let key = if missing.len() == 1 {
            "query.params.missing_value_one"
        } else {
            "query.params.missing_value_many"
        };
        return Err(crate::localization::format(
            key,
            &[("names", &missing.join(", "))],
        ));
    }

    let out = rewrite_code_tokens(db, sql, |remaining, out| {
        if !remaining.starts_with('@') {
            return None;
        }
        let name_len = remaining[1..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if name_len == 0 {
            return None;
        }
        let token = &remaining[..1 + name_len];
        if let Some((_, literal)) = resolved
            .iter()
            .find(|(spec, _)| spec.name.eq_ignore_ascii_case(token))
        {
            out.push_str(literal);
            Some(1 + name_len)
        } else {
            out.push_str(token);
            Some(1 + name_len)
        }
    });
    Ok(out)
}

#[derive(PartialEq)]
enum LexState {
    Code,
    Single,
    Double,
    Bracket,
    Backtick,
    LineComment,
    BlockComment(u32),
}

/// Single pass over SQL, skipping strings/comments/quoted names. In `Code`
/// state, `on_code` may consume a prefix of the remaining slice and append a
/// replacement; returning `None` copies one character. `[` opens a quoted
/// name only on bracket-quoting engines; elsewhere it is an array subscript.
fn rewrite_code_tokens<F>(db: DatabaseType, sql: &str, mut on_code: F) -> String
where
    F: FnMut(&str, &mut String) -> Option<usize>,
{
    let bracket_quotes = matches!(db.family(), Dialect::SqlServer | Dialect::Sqlite);
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0usize;
    let mut state = LexState::Code;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if state != LexState::Code {
            match state {
                LexState::Single if byte == b'\'' => {
                    if next == Some(b'\'') {
                        out.push_str("''");
                        index += 2;
                        continue;
                    }
                    state = LexState::Code;
                }
                LexState::Double if byte == b'"' => state = LexState::Code,
                LexState::Bracket if byte == b']' => state = LexState::Code,
                LexState::Backtick if byte == b'`' => state = LexState::Code,
                LexState::LineComment if byte == b'\n' => state = LexState::Code,
                LexState::BlockComment(depth) => match (byte, next) {
                    (b'/', Some(b'*')) => {
                        state = LexState::BlockComment(depth + 1);
                        out.push_str("/*");
                        index += 2;
                        continue;
                    }
                    (b'*', Some(b'/')) => {
                        state = if depth == 1 {
                            LexState::Code
                        } else {
                            LexState::BlockComment(depth - 1)
                        };
                        out.push_str("*/");
                        index += 2;
                        continue;
                    }
                    _ => {}
                },
                _ => {}
            }
            let ch_len = sql[index..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&sql[index..index + ch_len]);
            index += ch_len;
            continue;
        }
        match (byte, next) {
            (b'-', Some(b'-')) => {
                state = LexState::LineComment;
                out.push_str("--");
                index += 2;
                continue;
            }
            (b'/', Some(b'*')) => {
                state = LexState::BlockComment(1);
                out.push_str("/*");
                index += 2;
                continue;
            }
            (b'\'', _) => state = LexState::Single,
            (b'"', _) => state = LexState::Double,
            (b'[', _) if bracket_quotes => state = LexState::Bracket,
            (b'`', _) => state = LexState::Backtick,
            _ => {
                if let Some(consumed) = on_code(&sql[index..], &mut out) {
                    index += consumed;
                    continue;
                }
            }
        }
        let ch_len = sql[index..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&sql[index..index + ch_len]);
        index += ch_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A negative number substituted right after a `-` would form `--` and
    /// comment out the rest of the line (dropping, say, a tenant filter).
    #[test]
    fn negative_numbers_cannot_form_a_line_comment() {
        let sql = "-- sqlly param: @n int\n-- sqlly param: @d decimal\nSELECT 10 -@n, 1 -@d FROM t WHERE tenant = 3";
        let specs = parse_param_directives(sql);
        let out = substitute_params(
            DatabaseType::SqlServer,
            sql,
            &specs,
            &[("@n".into(), "-5".into()), ("@d".into(), "-1.5e2".into())],
        )
        .unwrap();
        let body = out.lines().last().unwrap();
        assert!(!body.contains("--"), "{body}");
        assert!(body.contains("10 -(-5)"), "{body}");
        assert!(body.contains("1 -(-1.5e2)"), "{body}");
        assert!(body.ends_with("WHERE tenant = 3"), "{body}");
    }

    #[test]
    fn parses_specs_with_types_and_defaults() {
        let spec = parse_spec(" @CustomerId int ").unwrap();
        assert_eq!(spec.name, "@CustomerId");
        assert_eq!(spec.ty, ParamType::Int);
        assert_eq!(spec.default, None);

        let spec = parse_spec(" @From date = 2026-01-01").unwrap();
        assert_eq!(spec.default.as_deref(), Some("2026-01-01"));

        assert!(parse_spec("CustomerId int").is_err());
        assert!(parse_spec("@x widget").is_err());
        assert!(parse_spec("@x int = notanumber").is_err());
        assert!(parse_spec("@x").is_err());
    }

    #[test]
    fn collects_directives_ignoring_duplicates_and_noise() {
        let sql = "-- sqlly param: @a int\n-- sqlly param: @b text = x\n-- sqlly param: @A int\nSELECT @a, @b";
        let specs = parse_param_directives(sql);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "@a");
        assert_eq!(specs[1].name, "@b");
    }

    #[test]
    fn substitutes_typed_literals_and_skips_strings_and_comments() {
        let sql = "-- sqlly param: @id int\n-- sqlly param: @name text\nSELECT '@id' AS literal, @id AS value, @idx, @name -- @id in a comment";
        let specs = parse_param_directives(sql);
        let out = substitute_params(
            DatabaseType::SqlServer,
            sql,
            &specs,
            &[
                ("@id".into(), "42".into()),
                ("@name".into(), "O'Brien".into()),
            ],
        )
        .unwrap();
        assert!(
            out.contains("SELECT '@id' AS literal, 42 AS value, @idx, N'O''Brien'"),
            "{out}"
        );
        assert!(out.contains("-- @id in a comment"), "{out}");
    }

    #[test]
    fn defaults_null_and_missing_values() {
        let sql =
            "-- sqlly param: @from date = 2026-01-01\n-- sqlly param: @to date\nSELECT @from, @to";
        let specs = parse_param_directives(sql);
        let err = substitute_params(DatabaseType::Postgres, sql, &specs, &[]).unwrap_err();
        assert!(err.contains("@to"), "{err}");

        let out = substitute_params(
            DatabaseType::Postgres,
            sql,
            &specs,
            &[("@to".into(), "NULL".into())],
        )
        .unwrap();
        assert!(out.contains("SELECT '2026-01-01', NULL"), "{out}");
    }

    #[test]
    fn value_validation_blocks_type_mismatches() {
        let sql = "-- sqlly param: @id int\nSELECT @id";
        let specs = parse_param_directives(sql);
        let err = substitute_params(
            DatabaseType::MySql,
            sql,
            &specs,
            &[("@id".into(), "1; DROP TABLE x".into())],
        )
        .unwrap_err();
        assert!(err.contains("@id"), "{err}");

        let out = substitute_params(
            DatabaseType::MySql,
            sql,
            &specs,
            &[("@id".into(), "7".into())],
        )
        .unwrap();
        assert!(out.contains("SELECT 7"), "{out}");
    }

    #[test]
    fn bool_literals_are_dialect_correct() {
        let sql = "-- sqlly param: @active bool\nSELECT @active";
        let specs = parse_param_directives(sql);
        let tds = substitute_params(
            DatabaseType::SqlServer,
            sql,
            &specs,
            &[("@active".into(), "true".into())],
        )
        .unwrap();
        assert!(tds.contains("SELECT 1"), "{tds}");
        let pg = substitute_params(
            DatabaseType::Postgres,
            sql,
            &specs,
            &[("@active".into(), "true".into())],
        )
        .unwrap();
        assert!(pg.contains("SELECT TRUE"), "{pg}");
    }

    #[test]
    fn parses_and_validates_extended_types() {
        assert_eq!(parse_spec("@u uuid").unwrap().ty, ParamType::Uuid);
        assert_eq!(parse_spec("@g guid").unwrap().ty, ParamType::Uuid);
        assert_eq!(parse_spec("@t time").unwrap().ty, ParamType::Time);
        assert_eq!(parse_spec("@j jsonb").unwrap().ty, ParamType::Json);
        assert_eq!(parse_spec("@b varbinary").unwrap().ty, ParamType::Binary);

        // UUID: with dashes, bare-32, rejects junk.
        assert!(validate_value(ParamType::Uuid, "550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_value(ParamType::Uuid, "550e8400e29b41d4a716446655440000").is_ok());
        assert!(validate_value(ParamType::Uuid, "not-a-uuid").is_err());

        // Time: hh:mm[:ss[.fff]] with range checks.
        assert!(validate_value(ParamType::Time, "09:30").is_ok());
        assert!(validate_value(ParamType::Time, "23:59:59.250").is_ok());
        assert!(validate_value(ParamType::Time, "24:00:00").is_err());
        assert!(validate_value(ParamType::Time, "9:30").is_err());

        // JSON: parseable only.
        assert!(validate_value(ParamType::Json, "{\"a\":1}").is_ok());
        assert!(validate_value(ParamType::Json, "[1,2,3]").is_ok());
        assert!(validate_value(ParamType::Json, "{oops}").is_err());

        // Binary: hex, optional 0x, even length.
        assert!(validate_value(ParamType::Binary, "DEAD").is_ok());
        assert!(validate_value(ParamType::Binary, "0xdead").is_ok());
        assert!(validate_value(ParamType::Binary, "DEA").is_err());
        assert!(validate_value(ParamType::Binary, "0xGG").is_err());

        // NULL is accepted for every extended type.
        for ty in [
            ParamType::Uuid,
            ParamType::Time,
            ParamType::Json,
            ParamType::Binary,
        ] {
            assert!(validate_value(ty, "NULL").is_ok());
        }
    }

    #[test]
    fn binary_param_renders_dialect_hex_literal() {
        let sql = "-- sqlly param: @b binary\nSELECT @b";
        let specs = parse_param_directives(sql);
        let tds = substitute_params(
            DatabaseType::SqlServer,
            sql,
            &specs,
            &[("@b".into(), "0xDEAD".into())],
        )
        .unwrap();
        assert!(tds.contains("SELECT 0xDEAD"), "{tds}");
        let sqlite = substitute_params(
            DatabaseType::Sqlite,
            sql,
            &specs,
            &[("@b".into(), "DEAD".into())],
        )
        .unwrap();
        assert!(sqlite.contains("SELECT X'DEAD'"), "{sqlite}");
    }

    #[test]
    fn uuid_and_json_params_render_as_string_literals() {
        let sql = "-- sqlly param: @u uuid\n-- sqlly param: @j json\nSELECT @u, @j";
        let specs = parse_param_directives(sql);
        let out = substitute_params(
            DatabaseType::Postgres,
            sql,
            &specs,
            &[
                ("@u".into(), "550e8400-e29b-41d4-a716-446655440000".into()),
                ("@j".into(), "{\"k\":1}".into()),
            ],
        )
        .unwrap();
        assert!(
            out.contains("'550e8400-e29b-41d4-a716-446655440000'"),
            "{out}"
        );
        assert!(out.contains("'{\"k\":1}'"), "{out}");
    }

    #[test]
    fn preserves_utf8_in_string_literals_without_substitution() {
        let sql = "SELECT 'café'";
        let out = substitute_params(DatabaseType::Postgres, sql, &[], &[]).unwrap();
        assert_eq!(out, sql);
    }

    #[test]
    fn preserves_utf8_in_line_comments() {
        let sql = "SELECT 1 -- 你好世界";
        let out = substitute_params(DatabaseType::Postgres, sql, &[], &[]).unwrap();
        assert_eq!(out, sql);
    }

    #[test]
    fn substitutes_params_while_preserving_utf8_strings() {
        let sql = "-- sqlly param: @name text\nSELECT @name, 'café'";
        let specs = parse_param_directives(sql);
        let out = substitute_params(
            DatabaseType::SqlServer,
            sql,
            &specs,
            &[("@name".into(), "Alice".into())],
        )
        .unwrap();
        assert!(out.contains("N'Alice'"), "{out}");
        assert!(out.contains("'café'"), "{out}");
    }

    // Lexer does not treat `N'` as a national-string prefix — `N` stays in Code and
    // only the quoted portion is a string literal. No separate N' test.

    #[test]
    fn first_unresolved_names_the_row_run_should_focus() {
        let sql = "-- sqlly param: @id int\n-- sqlly param: @name text = 'x'\n-- sqlly param: @day date\nSELECT @id, @name, @day";
        let specs = parse_param_directives(sql);
        // @id has no value and no default → first unresolved.
        assert_eq!(first_unresolved(&specs, &[]), Some("@id".to_string()));
        // @id valid, @name falls back to its default, @day invalid.
        let values = vec![
            ("@id".to_string(), "7".to_string()),
            ("@day".to_string(), "not-a-date".to_string()),
        ];
        assert_eq!(first_unresolved(&specs, &values), Some("@day".to_string()));
        // Everything resolves (default + NULL both count).
        let values = vec![
            ("@id".to_string(), "7".to_string()),
            ("@day".to_string(), "NULL".to_string()),
        ];
        assert_eq!(first_unresolved(&specs, &values), None);
    }

    #[test]
    fn set_spec_parses_and_rejects_bad_names() {
        let spec = parse_set_spec("schema = dbo").unwrap();
        assert_eq!(spec.name, "schema");
        assert_eq!(spec.value, "dbo");
        assert!(parse_set_spec("@x = 1").is_err());
        assert!(parse_set_spec("x").is_err());
        assert!(parse_set_spec("x = ").is_err());
    }

    #[test]
    fn set_last_assignment_wins() {
        let sql = "-- sqlly set a = 1\n-- sqlly set b = 2\n-- sqlly set a = 3\nSELECT 1";
        let vars = parse_set_directives(sql);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].name, "a");
        assert_eq!(vars[0].value, "3");
    }

    #[test]
    fn variables_skip_strings_comments_and_quoted_identifiers() {
        let sql = "SELECT '${a}', \"${a}\", [${a}], `${a}` -- ${a}";
        let vars = vec![VariableSpec {
            name: "a".into(),
            value: "x".into(),
        }];
        let out = substitute_variables(DatabaseType::SqlServer, sql, &vars);
        assert_eq!(out, sql);
        let out = substitute_variables(DatabaseType::SqlServer, "WHERE x = ${a}", &vars);
        assert_eq!(out, "WHERE x = x");
    }

    // DBeaver #42360, DataGrip DBE-25057 / DBE-18646 / DBE-27337, Beekeeper
    // #4702: only `-- sqlly param:` / `-- sqlly set` lines declare anything,
    // so placeholders in comments, colons and plain queries never ask for a
    // value and run exactly as written.
    #[test]
    fn undeclared_placeholders_are_never_parameters() {
        for sql in [
            "-- ${timestamp}\n\nSELECT 1;",
            "-- @export {\"type\":\"csv\",\"consumer\":{\"outputFilePattern\":\"user-template_${timestamp}\"}}\n\nSELECT 1;",
            "WITH data AS (SELECT ARRAY[10, 20, 30, 40] AS my_array, 1 AS min_bound, 3 AS max_bound)\nSELECT my_array[min_bound:max_bound] FROM data;",
            "select metadata:country from mytable;",
            "select u.usrRegistration, r.Nombre as devAmicoDevId\n  from hid_ta_users_devices ud\n  join rcp_taNRelojes r on (r.RelojId = ud.usdDeviceId)\n  --where usdStatus = 0\n  order by usrRegistration",
            "SELECT :name, ?, $1, @local FROM t",
        ] {
            assert!(parse_param_directives(sql).is_empty(), "{sql}");
            assert!(parse_set_directives(sql).is_empty(), "{sql}");
        }
    }

    // DataGrip DBE-25057: `[` opens a quoted name only on bracket-quoting
    // engines, so array slices elsewhere still receive their values.
    #[test]
    fn array_subscripts_substitute_outside_bracket_quoting_engines() {
        let sql =
            "-- sqlly param: @lo int\n-- sqlly param: @hi int\nSELECT arr[@lo:@hi], [@lo] FROM t";
        let specs = parse_param_directives(sql);
        let values = [("@lo".into(), "1".into()), ("@hi".into(), "3".into())];
        let pg = substitute_params(DatabaseType::Postgres, sql, &specs, &values).unwrap();
        assert!(pg.contains("SELECT arr[1:3], [1] FROM t"), "{pg}");
        let mssql = substitute_params(DatabaseType::SqlServer, sql, &specs, &values).unwrap();
        assert!(
            mssql.contains("SELECT arr[@lo:@hi], [@lo] FROM t"),
            "{mssql}"
        );

        let vars = vec![
            VariableSpec {
                name: "lo".into(),
                value: "2".into(),
            },
            VariableSpec {
                name: "hi".into(),
                value: "4".into(),
            },
        ];
        let sql = "SELECT arr[${lo}:${hi}] FROM t";
        assert_eq!(
            substitute_variables(DatabaseType::Postgres, sql, &vars),
            "SELECT arr[2:4] FROM t"
        );
        assert_eq!(substitute_variables(DatabaseType::Sqlite, sql, &vars), sql);
    }

    #[test]
    fn variables_leave_unknown_placeholders() {
        let out = substitute_variables(DatabaseType::SqlServer, "WHERE x = ${missing}", &[]);
        assert_eq!(out, "WHERE x = ${missing}");
    }

    #[test]
    fn variables_run_before_params() {
        let full = "-- sqlly set p = @id\n-- sqlly param: @id int\nWHERE x = ${p}";
        let vars = parse_set_directives(full);
        let sql = substitute_variables(DatabaseType::SqlServer, "WHERE x = ${p}", &vars);
        let specs = parse_param_directives(full);
        let out = substitute_params(
            DatabaseType::SqlServer,
            &sql,
            &specs,
            &[("@id".into(), "42".into())],
        )
        .unwrap();
        assert_eq!(out, "WHERE x = 42");
    }

    #[test]
    fn variables_are_utf8_safe() {
        let vars = vec![VariableSpec {
            name: "greeting".into(),
            value: "café".into(),
        }];
        let out = substitute_variables(
            DatabaseType::SqlServer,
            "SELECT '${greeting}' /* ${greeting} */ ${greeting} 🙂",
            &vars,
        );
        assert_eq!(out, "SELECT '${greeting}' /* ${greeting} */ café 🙂");
    }
}
