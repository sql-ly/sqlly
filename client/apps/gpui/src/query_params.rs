//! Query parameters: `-- sqlly param: @name <type> [= default]` directives,
//! and the `:name` / `?` / `$1` placeholders written straight into the SQL.
//!
//! Both surface as a strip of inputs; Run substitutes each occurrence
//! (outside strings, comments, and quoted names) with a validated,
//! dialect-correct literal before the SQL leaves the app. A placeholder with
//! no directive is typed [`ParamType::Auto`]: its value is read as a number,
//! boolean, NULL, or text the way a person would mean it. Pure — the
//! parsing, validation, and substitution carry no GPUI types.

use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

/// The value families a parameter can declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    /// An undeclared placeholder: the value decides (see [`auto_literal`]).
    Auto,
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
            Self::Auto => "auto",
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
            "auto" | "any" => Self::Auto,
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
                    "unknown type '{other}' — use auto, text, int, decimal, bool, date, \
                     datetime, time, uuid, json, or binary"
                ));
            }
        })
    }
}

/// One declared parameter, or one placeholder found in the SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    /// Includes the sigil: `@name` and `:name` from a directive or the SQL,
    /// `?1` / `$1` for positional placeholders.
    pub name: String,
    pub ty: ParamType,
    pub default: Option<String>,
}

/// Parse one directive body (the text after `param:`).
pub fn parse_spec(body: &str) -> Result<ParamSpec, String> {
    let body = body.trim();
    // `@name` substitutes on its own; `:name` types a placeholder of the
    // same name written in the SQL.
    let sigil = body
        .chars()
        .next()
        .filter(|c| matches!(c, '@' | ':'))
        .ok_or_else(|| crate::localization::text("query.params.name_requires_at"))?;
    let rest = &body[1..];
    let name_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count();
    if name_len == 0 || rest.as_bytes()[0].is_ascii_digit() {
        return Err(crate::localization::text("query.params.name_requires_at"));
    }
    let name = format!("{sigil}{}", &rest[..name_len]);
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
        ParamType::Auto | ParamType::Text => true,
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
        ParamType::Auto => auto_literal(db, raw),
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
    // `:name` directives type placeholders; only `@name` ones substitute here.
    let specs: Vec<&ParamSpec> = specs.iter().filter(|s| s.name.starts_with('@')).collect();
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
    /// `'…'`; `true` when a backslash escapes the next character (MySQL and
    /// ClickHouse strings, PostgreSQL `E'…'`).
    Single(bool),
    Double(bool),
    Bracket,
    Backtick,
    LineComment,
    BlockComment(u32),
}

/// The engine's quoting rules that decide where code ends and data begins.
struct LexRules {
    bracket_quotes: bool,
    backslash_escapes: bool,
    hash_comments: bool,
    dollar_quotes: bool,
}

impl LexRules {
    fn for_db(db: DatabaseType) -> Self {
        use sqlly_lang::sqlmodel::ConnectionKind;
        let kind = db.connection_kind();
        let mysql_like = matches!(
            kind,
            ConnectionKind::MySqlWire | ConnectionKind::ClickHouseHttp
        );
        Self {
            bracket_quotes: matches!(db.family(), Dialect::SqlServer | Dialect::Sqlite),
            backslash_escapes: mysql_like,
            hash_comments: mysql_like,
            dollar_quotes: kind == ConnectionKind::PostgresWire,
        }
    }
}

/// The length of a PostgreSQL dollar-quote opener (`$$`, `$body$`) at the
/// start of `rest`, or `None` when `rest` does not open one. A tag never
/// starts with a digit, so `$1` stays a positional parameter.
fn dollar_quote_tag(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let tag_len = bytes[1..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    if tag_len > 0 && bytes[1].is_ascii_digit() {
        return None;
    }
    (bytes.get(1 + tag_len) == Some(&b'$')).then_some(tag_len + 2)
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// Single pass over SQL, skipping strings/comments/quoted names. In `Code`
/// state, `on_code` may consume a prefix of the remaining slice and append a
/// replacement; returning `None` copies one character. `[` opens a quoted
/// name only on bracket-quoting engines; elsewhere it is an array subscript.
/// MySQL-family strings honor backslash escapes and `#` comments, and
/// PostgreSQL dollar-quoted bodies are copied whole, so nothing inside a
/// function body or an escaped quote is ever rewritten.
fn rewrite_code_tokens<F>(db: DatabaseType, sql: &str, mut on_code: F) -> String
where
    F: FnMut(&str, &mut String) -> Option<usize>,
{
    let rules = LexRules::for_db(db);
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0usize;
    let mut state = LexState::Code;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if state != LexState::Code {
            match state {
                LexState::Single(true) | LexState::Double(true) if byte == b'\\' => {
                    let escaped = sql[index + 1..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    out.push_str(&sql[index..index + 1 + escaped]);
                    index += 1 + escaped;
                    continue;
                }
                LexState::Single(_) if byte == b'\'' => {
                    if next == Some(b'\'') {
                        out.push_str("''");
                        index += 2;
                        continue;
                    }
                    state = LexState::Code;
                }
                LexState::Double(_) if byte == b'"' => {
                    if next == Some(b'"') {
                        out.push_str("\"\"");
                        index += 2;
                        continue;
                    }
                    state = LexState::Code;
                }
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
        let prev = index.checked_sub(1).map(|i| bytes[i]);
        match (byte, next) {
            (b'-', Some(b'-')) => {
                state = LexState::LineComment;
                out.push_str("--");
                index += 2;
                continue;
            }
            (b'#', _) if rules.hash_comments => state = LexState::LineComment,
            (b'/', Some(b'*')) => {
                state = LexState::BlockComment(1);
                out.push_str("/*");
                index += 2;
                continue;
            }
            (b'\'', _) => {
                // PostgreSQL `E'…'` is the escape-string form.
                let e_string = rules.dollar_quotes
                    && matches!(prev, Some(b'E' | b'e'))
                    && index
                        .checked_sub(2)
                        .is_none_or(|i| !is_ident_byte(bytes[i]));
                state = LexState::Single(rules.backslash_escapes || e_string);
            }
            (b'"', _) => state = LexState::Double(rules.backslash_escapes),
            (b'[', _) if rules.bracket_quotes => state = LexState::Bracket,
            (b'`', _) => state = LexState::Backtick,
            (b'$', _)
                if rules.dollar_quotes && prev.is_none_or(|b| !is_ident_byte(b) && b != b'$') =>
            {
                if let Some(open_len) = dollar_quote_tag(&sql[index..]) {
                    let tag = &sql[index..index + open_len];
                    let body_start = index + open_len;
                    let close = sql[body_start..]
                        .find(tag)
                        .map_or(sql.len(), |at| body_start + at + open_len);
                    out.push_str(&sql[index..close]);
                    index = close;
                    continue;
                }
                if let Some(consumed) = on_code(&sql[index..], &mut out) {
                    index += consumed;
                    continue;
                }
            }
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

/// One placeholder written straight into the SQL. `key` names it in the
/// strip: `:id` and `@id` as written, `?1` for the first plain `?` (SQLite's
/// `?3` keeps its number), `$2` as written. `start..end` is its byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub key: String,
    pub start: usize,
    pub end: usize,
}

/// Every `:name`, `?`, `?NNN`, and `$N` placeholder in `sql`, in order, and
/// on SQL Server every `@name` that nothing in the script binds (see
/// [`tsql_bound_variables`]).
///
/// Only code counts: strings, comments, quoted names, and dollar-quoted
/// bodies are skipped. `:name` must not touch a name, `:` or closing bracket
/// on its left, so `a::int` casts, `arr[lo:hi]` slices, `metadata:country`
/// paths, MySQL `:=` and `lbl:LOOP` labels are left alone, and so are a SQL
/// Server sqlcmd line (`:setvar`) and Oracle trigger `:new` / `:old`. `?` is
/// an operator on PostgreSQL (`jsonb ? 'key'`), which uses `$1` instead.
/// Redis commands have no placeholders.
pub fn find_placeholders(db: DatabaseType, sql: &str) -> Vec<Placeholder> {
    find_placeholders_after(db, sql, 0, sql)
}

/// [`find_placeholders`] for a region of a larger buffer whose earlier text
/// already numbered `positional_before` plain `?` placeholders, so a
/// statement run on its own reads `?` the way the whole-buffer strip lists
/// it. `buffer` is the whole script: a SQL Server variable it declares
/// anywhere is never a placeholder, even in a statement run on its own.
pub fn find_placeholders_after(
    db: DatabaseType,
    sql: &str,
    positional_before: u32,
    buffer: &str,
) -> Vec<Placeholder> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let kind = db.connection_kind();
    if kind == ConnectionKind::Resp || !sql.contains([':', '?', '$', '@']) {
        return Vec::new();
    }
    let at_bound = if kind == ConnectionKind::Tds && sql.contains('@') {
        tsql_bound_variables(db, buffer)
    } else {
        None
    };
    let mut in_exec = false;
    let question_marks = kind != ConnectionKind::PostgresWire;
    let dollar_numbers = matches!(
        kind,
        ConnectionKind::PostgresWire | ConnectionKind::DuckDbFile
    );
    let sqlcmd_lines = db.family() == Dialect::SqlServer;
    let oracle = kind == ConnectionKind::OracleNet;
    let bytes = sql.as_bytes();
    let mut found = Vec::new();
    let mut highest = positional_before;
    rewrite_code_tokens(db, sql, |remaining, _out| {
        let start = sql.len() - remaining.len();
        let prev = start.checked_sub(1).map(|i| bytes[i]);
        let rest = remaining.as_bytes();
        match rest[0] {
            b':' => {
                let name_len = remaining[1..]
                    .char_indices()
                    .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                    .last()
                    .map_or(0, |(i, c)| i + c.len_utf8());
                let starts_name = remaining[1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_');
                let touches_left = prev.is_some_and(|b| {
                    is_ident_byte(b) || matches!(b, b':' | b']' | b')' | b'"' | b'`' | b'\'')
                });
                if !starts_name || touches_left {
                    // Step over a `::` run whole so its second colon is
                    // never read as the start of a name.
                    return (rest.get(1) == Some(&b':')).then_some(2);
                }
                let name = &remaining[1..1 + name_len];
                let line_start = sql[..start].rfind('\n').map_or(0, |i| i + 1);
                if (sqlcmd_lines && sql[line_start..start].trim().is_empty())
                    || (oracle
                        && (name.eq_ignore_ascii_case("new") || name.eq_ignore_ascii_case("old")))
                {
                    return Some(1 + name_len);
                }
                found.push(Placeholder {
                    key: format!(":{name}"),
                    start,
                    end: start + 1 + name_len,
                });
                Some(1 + name_len)
            }
            b'?' if question_marks && !matches!(rest.get(1), Some(b'|' | b'&')) => {
                let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                let number = if digits > 0 {
                    remaining[1..1 + digits].parse::<u32>().unwrap_or(u32::MAX)
                } else {
                    highest.saturating_add(1)
                };
                highest = highest.max(number);
                found.push(Placeholder {
                    key: format!("?{number}"),
                    start,
                    end: start + 1 + digits,
                });
                Some(1 + digits)
            }
            b'@' if at_bound.is_some() => {
                if rest.get(1) == Some(&b'@') {
                    return Some(2 + tsql_name_len(&rest[2..]));
                }
                let name_len = tsql_name_len(&rest[1..]);
                if name_len == 0 || prev.is_some_and(is_ident_byte) {
                    return (name_len > 0).then_some(1 + name_len);
                }
                let name = &remaining[1..1 + name_len];
                let bound = at_bound
                    .as_ref()
                    .is_some_and(|bound| bound.contains(&name.to_ascii_lowercase()));
                // `EXEC dbo.p @CustomerId = …` names the procedure's own
                // parameter; only the value side can be a placeholder.
                let exec_argument =
                    in_exec && remaining[1 + name_len..].trim_start().starts_with('=');
                if !bound && !exec_argument {
                    found.push(Placeholder {
                        key: remaining[..1 + name_len].to_string(),
                        start,
                        end: start + 1 + name_len,
                    });
                }
                Some(1 + name_len)
            }
            b';' if at_bound.is_some() => {
                in_exec = false;
                None
            }
            c if at_bound.is_some()
                && (c.is_ascii_alphabetic() || c == b'_')
                && prev.is_none_or(|b| !is_ident_byte(b) && !matches!(b, b'#' | b'$')) =>
            {
                let len = tsql_name_len(rest);
                let word = remaining[..len].to_ascii_uppercase();
                if is_tsql_statement_start(&word) {
                    in_exec = matches!(word.as_str(), "EXEC" | "EXECUTE");
                }
                Some(len)
            }
            b'$' if dollar_numbers && prev.is_none_or(|b| !is_ident_byte(b) && b != b'$') => {
                let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                if digits == 0 || rest.get(1 + digits).is_some_and(|b| is_ident_byte(*b)) {
                    return None;
                }
                found.push(Placeholder {
                    key: remaining[..1 + digits].to_string(),
                    start,
                    end: start + 1 + digits,
                });
                Some(1 + digits)
            }
            _ => None,
        }
    });
    found
}

fn tsql_name_len(rest: &[u8]) -> usize {
    rest.iter()
        .take_while(|b| is_ident_byte(**b) || matches!(**b, b'$' | b'#'))
        .count()
}

/// T-SQL words that begin a new statement, ending a `DECLARE` list or an
/// `EXEC` argument list that was not closed with `;`.
fn is_tsql_statement_start(word: &str) -> bool {
    matches!(
        word,
        "SELECT"
            | "SET"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "MERGE"
            | "EXEC"
            | "EXECUTE"
            | "IF"
            | "ELSE"
            | "WHILE"
            | "BEGIN"
            | "END"
            | "RETURN"
            | "WITH"
            | "PRINT"
            | "RAISERROR"
            | "THROW"
            | "GO"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "TRUNCATE"
            | "OPEN"
            | "FETCH"
            | "CLOSE"
            | "DEALLOCATE"
            | "USE"
            | "DECLARE"
            | "WAITFOR"
            | "BREAK"
            | "CONTINUE"
            | "GRANT"
            | "REVOKE"
            | "DENY"
            | "COMMIT"
            | "ROLLBACK"
            | "SAVE"
            | "BACKUP"
            | "RESTORE"
            | "DBCC"
    )
}

/// The SQL Server variables a script binds itself, lowercased: every name in
/// a `DECLARE` list (`DECLARE @a int, @t TABLE (…)`), and the targets of
/// `SET @x`, `SELECT @x =`, `EXEC @rc =` and `FETCH … INTO @x`. `None` when
/// the script defines a procedure, function or trigger: its `@` names are
/// the routine's own parameters and locals, so none of them is a
/// placeholder (Procedure mode fills those).
fn tsql_bound_variables(
    db: DatabaseType,
    buffer: &str,
) -> Option<std::collections::HashSet<String>> {
    let mut bound = std::collections::HashSet::new();
    let mut routine = false;
    let mut in_declare = false;
    let mut expect_name = false;
    let mut depth = 0i32;
    let mut prev_word = String::new();
    let bytes = buffer.as_bytes();
    rewrite_code_tokens(db, buffer, |remaining, _out| {
        let start = buffer.len() - remaining.len();
        let prev = start.checked_sub(1).map(|i| bytes[i]);
        let rest = remaining.as_bytes();
        match rest[0] {
            b'@' => {
                if rest.get(1) == Some(&b'@') {
                    return Some(2 + tsql_name_len(&rest[2..]));
                }
                let len = tsql_name_len(&rest[1..]);
                if len == 0 {
                    return None;
                }
                let name = remaining[1..1 + len].to_ascii_lowercase();
                let assigned = remaining[1 + len..].trim_start().starts_with('=');
                let declared = in_declare && expect_name && depth == 0;
                let target = matches!(prev_word.as_str(), "SET" | "INTO")
                    || (assigned && matches!(prev_word.as_str(), "SELECT" | "EXEC" | "EXECUTE"));
                if declared || target {
                    bound.insert(name);
                }
                expect_name = false;
                prev_word.clear();
                Some(1 + len)
            }
            b'(' => {
                depth += 1;
                None
            }
            b')' => {
                depth -= 1;
                None
            }
            b',' => {
                if in_declare && depth == 0 {
                    expect_name = true;
                }
                None
            }
            b';' => {
                in_declare = false;
                expect_name = false;
                depth = 0;
                None
            }
            c if (c.is_ascii_alphabetic() || c == b'_')
                && prev.is_none_or(|b| !is_ident_byte(b) && !matches!(b, b'#' | b'$')) =>
            {
                let len = tsql_name_len(rest);
                let word = remaining[..len].to_ascii_uppercase();
                if matches!(word.as_str(), "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER")
                    && matches!(prev_word.as_str(), "CREATE" | "ALTER")
                {
                    routine = true;
                }
                if word == "DECLARE" {
                    in_declare = true;
                    expect_name = true;
                    depth = 0;
                } else if depth == 0 && is_tsql_statement_start(&word) {
                    in_declare = false;
                    expect_name = false;
                }
                prev_word = word;
                Some(len)
            }
            _ => None,
        }
    });
    (!routine).then_some(bound)
}

/// How many plain `?` numbers the text before `offset` used up (the highest
/// `?N` seen), so a run of `buffer[offset..]` numbers its own `?` on.
pub fn positional_before(db: DatabaseType, buffer: &str, offset: usize) -> u32 {
    let Some(prefix) = buffer.get(..offset) else {
        return 0;
    };
    find_placeholders(db, prefix)
        .iter()
        .filter_map(|p| p.key.strip_prefix('?')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

/// The strip's rows for a buffer: every `-- sqlly param:` declaration, then
/// each placeholder found in the SQL that no directive declares, once per
/// key in first-appearance order (`:id` and `:ID` are one parameter).
pub fn strip_specs(db: DatabaseType, sql: &str, detect_placeholders: bool) -> Vec<ParamSpec> {
    let mut specs = parse_param_directives(sql);
    if detect_placeholders {
        for placeholder in find_placeholders(db, sql) {
            if !specs
                .iter()
                .any(|spec| spec.name.eq_ignore_ascii_case(&placeholder.key))
            {
                specs.push(ParamSpec {
                    name: placeholder.key,
                    ty: ParamType::Auto,
                    default: None,
                });
            }
        }
    }
    specs
}

/// Why Run could not substitute: the message for the status bar, and the
/// strip row to focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamRunError {
    pub message: String,
    pub focus: Option<String>,
}

/// Replace each placeholder (offsets into `sql`) with its value's literal.
/// A placeholder's type and default come from a `-- sqlly param: :name`
/// directive when there is one, else it is [`ParamType::Auto`]. Every value
/// is resolved before any text changes, so a missing or invalid one blocks
/// the whole run and names the first row to fix.
pub fn substitute_placeholders(
    db: DatabaseType,
    sql: &str,
    placeholders: &[Placeholder],
    declared: &[ParamSpec],
    values: &[(String, String)],
) -> Result<String, ParamRunError> {
    if placeholders.is_empty() {
        return Ok(sql.to_string());
    }
    let mut resolved: Vec<(&str, String)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for placeholder in placeholders {
        let key = placeholder.key.as_str();
        if resolved.iter().any(|(k, _)| k.eq_ignore_ascii_case(key))
            || missing.iter().any(|k| k.eq_ignore_ascii_case(key))
        {
            continue;
        }
        let spec = declared
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(key));
        let ty = spec.map_or(ParamType::Auto, |spec| spec.ty);
        let supplied = values
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty());
        match supplied.or(spec.and_then(|spec| spec.default.as_deref())) {
            Some(raw) => {
                validate_value(ty, raw).map_err(|err| ParamRunError {
                    message: crate::localization::format(
                        "query.params.named_value_invalid",
                        &[("name", key), ("error", &err)],
                    ),
                    focus: Some(key.to_string()),
                })?;
                resolved.push((key, literal(db, ty, raw)));
            }
            None => missing.push(key),
        }
    }
    if let Some(first) = missing.first() {
        let key = if missing.len() == 1 {
            "query.params.missing_value_one"
        } else {
            "query.params.missing_value_many"
        };
        return Err(ParamRunError {
            message: crate::localization::format(key, &[("names", &missing.join(", "))]),
            focus: Some(first.to_string()),
        });
    }
    let mut out = String::with_capacity(sql.len() + 16);
    let mut copied = 0;
    for placeholder in placeholders {
        let Some((_, literal)) = resolved
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&placeholder.key))
        else {
            continue;
        };
        out.push_str(&sql[copied..placeholder.start]);
        // Keep a value from fusing with a word it is written against
        // (`LIMIT?` → `LIMIT 10`, not `LIMIT10`).
        let fuses = |b: Option<u8>, edge: Option<char>| {
            b.is_some_and(is_ident_byte) && edge.is_some_and(|c| c.is_alphanumeric() || c == '_')
        };
        if fuses(out.as_bytes().last().copied(), literal.chars().next()) {
            out.push(' ');
        }
        out.push_str(literal);
        if fuses(
            sql.as_bytes().get(placeholder.end).copied(),
            literal.chars().last(),
        ) {
            out.push(' ');
        }
        copied = placeholder.end;
    }
    out.push_str(&sql[copied..]);
    Ok(out)
}

/// The literal for an undeclared placeholder's value, read the way a person
/// means it: `NULL` is SQL NULL, `true` / `false` are booleans, a plain
/// number is a number (a leading-zero code such as `007` stays text), and
/// anything else is text. Wrapping the value in single quotes (`'42'`,
/// `''`) forces text. On PostgreSQL every non-NULL value is an untyped
/// string literal, which the server reads as whatever type the comparison
/// needs (`int_col = '42'` and `text_col = '42'` both work, where a bare
/// `42` against text fails).
pub fn auto_literal(db: DatabaseType, raw: &str) -> String {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return db.string_literal(&raw[1..raw.len() - 1].replace("''", "'"));
    }
    if db.connection_kind() == ConnectionKind::PostgresWire {
        return db.string_literal(raw);
    }
    if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("false") {
        return db
            .bool_literal(raw.eq_ignore_ascii_case("true"))
            .to_string();
    }
    if is_plain_number(raw) {
        return if raw.starts_with('-') {
            format!("({raw})")
        } else {
            raw.to_string()
        };
    }
    db.string_literal(raw)
}

/// `42`, `-3.5`, `1e6`, `.5` — but not `007`, `1_000`, `0x1F`, or `+-1`.
fn is_plain_number(raw: &str) -> bool {
    let unsigned = raw.strip_prefix(['-', '+']).unwrap_or(raw);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let whole_ok = digits(whole) || (whole.is_empty() && fraction.is_some_and(digits));
    let leading_zero = whole.len() > 1 && whole.starts_with('0');
    let fraction_ok = fraction.is_none_or(|f| f.is_empty() && !whole.is_empty() || digits(f));
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)));
    whole_ok && !leading_zero && fraction_ok && exponent_ok
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

    // #4702 again, for placeholders found in the SQL: slices, colon paths,
    // comments, and T-SQL variables never ask for a value. Only a real
    // placeholder does.
    #[test]
    fn colon_lookalikes_are_not_placeholders() {
        for (db, sql) in [
            (
                DatabaseType::Postgres,
                "WITH data AS (SELECT ARRAY[10, 20, 30, 40] AS my_array, 1 AS min_bound, 3 AS max_bound)\nSELECT my_array[min_bound:max_bound] FROM data;",
            ),
            (
                DatabaseType::Snowflake,
                "select metadata:country from mytable;",
            ),
            (
                DatabaseType::SqlServer,
                "select u.usrRegistration\n  from hid_ta_users_devices ud\n  --where usdStatus = :x\n  order by usrRegistration",
            ),
            (
                DatabaseType::SqlServer,
                "DECLARE @local int = 1; SELECT @local, @@spid",
            ),
            (
                DatabaseType::SqlServer,
                ":setvar db Sales\nSELECT geography::Point(1, 2, 4326)",
            ),
            (
                DatabaseType::Postgres,
                "SELECT id::int, now()::date, data ? 'k', data ?| array['a'] FROM t",
            ),
            (
                DatabaseType::Postgres,
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT :x, $1 $$ LANGUAGE sql",
            ),
            (
                DatabaseType::Postgres,
                "SELECT E'it\\'s :x', $tag$ :y $tag$",
            ),
            (
                DatabaseType::MySql,
                "SELECT 'it\\'s :x', \"a :y\", `c:z` # trailing :w\nFROM t",
            ),
            (
                DatabaseType::MySql,
                "SET @n := 1; lbl:LOOP LEAVE lbl; END LOOP",
            ),
            (DatabaseType::MySql, "SELECT '12:30', '?' FROM t /* :c ? */"),
            (
                DatabaseType::Oracle,
                "CREATE TRIGGER t BEFORE UPDATE ON x FOR EACH ROW BEGIN :new.a := :old.a; END;",
            ),
            (DatabaseType::Redis, "GET key:1?"),
        ] {
            assert!(
                find_placeholders(db, sql).is_empty(),
                "{db:?}: {sql} -> {:?}",
                find_placeholders(db, sql)
            );
        }
    }

    #[test]
    fn finds_named_positional_and_numbered_placeholders() {
        let keys = |db, sql| -> Vec<String> {
            find_placeholders(db, sql)
                .into_iter()
                .map(|p| p.key)
                .collect()
        };
        assert_eq!(
            keys(
                DatabaseType::MariaDb,
                "SELECT * FROM beneficiaries WHERE id = :id;"
            ),
            [":id"]
        );
        assert_eq!(
            keys(
                DatabaseType::MySql,
                "WHERE a=:a AND b IN (?, ?) AND c=:a LIMIT ?"
            ),
            [":a", "?1", "?2", ":a", "?3"]
        );
        assert_eq!(
            keys(
                DatabaseType::Postgres,
                "WHERE a = $1 AND b = $2 OR c = $1 AND d = :d"
            ),
            ["$1", "$2", "$1", ":d"]
        );
        // SQLite numbers a plain `?` after the highest so far.
        assert_eq!(
            keys(DatabaseType::Sqlite, "WHERE a = ?3 AND b = ? AND c = :c"),
            ["?3", "?4", ":c"]
        );
        assert_eq!(
            keys(DatabaseType::SqlServer, "WHERE id = :id AND x = ?"),
            [":id", "?1"]
        );
        assert_eq!(
            keys(DatabaseType::DuckDb, "WHERE a = $1 AND b = ?"),
            ["$1", "?1"]
        );
        // Byte ranges point at the placeholder text.
        let sql = "SELECT 'café', :naïve";
        let found = find_placeholders(DatabaseType::Sqlite, sql);
        assert_eq!(&sql[found[0].start..found[0].end], ":naïve");
    }

    #[test]
    fn sql_server_at_names_nothing_declares_are_placeholders() {
        let keys = |sql| -> Vec<String> {
            find_placeholders(DatabaseType::SqlServer, sql)
                .into_iter()
                .map(|p| p.key)
                .collect()
        };
        assert_eq!(
            keys("SELECT * FROM dbo.Orders WHERE CustomerID = @CustomerId AND OrderDate >= @From"),
            ["@CustomerId", "@From"]
        );
        // Declared variables, table variables, assignment targets, system
        // functions, strings, comments and bracketed names are not.
        assert_eq!(
            keys(
                "DECLARE @n int = 5, @label nvarchar(20);\n\
                 DECLARE @t TABLE (id int, name nvarchar(10));\n\
                 SET @label = N'@x';\n\
                 SELECT @n = COUNT(*) FROM t -- @y\n\
                 SELECT @@ROWCOUNT, [@z], @n, @label FROM @t WHERE id = @Id"
            ),
            ["@Id"]
        );
        // A declaration elsewhere in the script still counts.
        let buffer = "DECLARE @n int = 1;\nSELECT * FROM t WHERE a = @n AND b = @b;";
        let start = buffer.find("SELECT").unwrap();
        let found = find_placeholders_after(DatabaseType::SqlServer, &buffer[start..], 0, buffer);
        assert_eq!(
            found.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            ["@b"]
        );
        // EXEC names the procedure's own parameters; the values can be
        // placeholders.
        assert_eq!(
            keys("EXEC dbo.CustomerOrderHistory @CustomerID = @Customer, @Top = 10"),
            ["@Customer"]
        );
        // A routine definition's @ names are its parameters and locals.
        assert!(
            keys("CREATE OR ALTER PROCEDURE dbo.p @id int AS SELECT * FROM t WHERE id = @id")
                .is_empty()
        );
        // Other engines keep their own @ meaning.
        assert!(find_placeholders(DatabaseType::MySql, "SELECT @total").is_empty());
    }

    #[test]
    fn sql_server_at_placeholder_runs_with_its_value() {
        let sql = "SELECT * FROM dbo.Customers WHERE Country = @Country AND Active = @Active";
        let found = find_placeholders(DatabaseType::SqlServer, sql);
        let out = substitute_placeholders(
            DatabaseType::SqlServer,
            sql,
            &found,
            &[],
            &[
                ("@country".into(), "O'Brien".into()),
                ("@Active".into(), "true".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "SELECT * FROM dbo.Customers WHERE Country = N'O''Brien' AND Active = 1"
        );
    }

    #[test]
    fn strip_lists_each_placeholder_once_after_directives() {
        let sql = "-- sqlly param: @limit int = 10\n-- sqlly param: :since date\n\
                   SELECT * FROM t WHERE created >= :since AND id = :ID AND x = :id AND y = ?";
        let specs = strip_specs(DatabaseType::MySql, sql, true);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["@limit", ":since", ":ID", "?1"]);
        assert_eq!(specs[1].ty, ParamType::Date);
        assert_eq!(specs[2].ty, ParamType::Auto);
        assert_eq!(strip_specs(DatabaseType::MySql, sql, false).len(), 2);
    }

    #[test]
    fn auto_values_read_the_way_people_mean_them() {
        let my = DatabaseType::MySql;
        assert_eq!(auto_literal(my, "42"), "42");
        assert_eq!(auto_literal(my, "-3.5"), "(-3.5)");
        assert_eq!(auto_literal(my, "1e6"), "1e6");
        assert_eq!(auto_literal(my, "007"), "'007'");
        assert_eq!(auto_literal(my, "0.5"), "0.5");
        assert_eq!(auto_literal(my, "null"), "NULL");
        assert_eq!(auto_literal(my, "TRUE"), "1");
        assert_eq!(auto_literal(my, "'42'"), "'42'");
        assert_eq!(auto_literal(my, "''"), "''");
        assert_eq!(auto_literal(my, "it's"), "'it''s'");
        assert_eq!(auto_literal(my, "back\\slash"), "'back\\\\slash'");
        assert_eq!(auto_literal(DatabaseType::SqlServer, "Zoë"), "N'Zoë'");
        assert_eq!(auto_literal(DatabaseType::SqlServer, "true"), "1");
        // PostgreSQL: untyped literals coerce to the column's type.
        assert_eq!(auto_literal(DatabaseType::Postgres, "42"), "'42'");
        assert_eq!(auto_literal(DatabaseType::Postgres, "NULL"), "NULL");
    }

    #[test]
    fn substitution_cannot_be_escaped_or_fuse_words() {
        let sql = "SELECT * FROM t WHERE name = :n AND tenant = 3 LIMIT?";
        let found = find_placeholders(DatabaseType::MySql, sql);
        let out = substitute_placeholders(
            DatabaseType::MySql,
            sql,
            &found,
            &[],
            &[
                (":n".into(), "x' OR '1'='1".into()),
                ("?1".into(), "5".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "SELECT * FROM t WHERE name = 'x'' OR ''1''=''1' AND tenant = 3 LIMIT 5"
        );
        // A trailing backslash cannot eat the closing quote on MySQL.
        let out = substitute_placeholders(
            DatabaseType::MySql,
            "WHERE a = :a AND tenant = 3",
            &find_placeholders(DatabaseType::MySql, "WHERE a = :a AND tenant = 3"),
            &[],
            &[(":a".into(), "x\\".into())],
        )
        .unwrap();
        assert_eq!(out, "WHERE a = 'x\\\\' AND tenant = 3");
        // A negative value after `-` never forms a `--` comment.
        let sql = "SELECT 10 -:n, 1 AS keep";
        let out = substitute_placeholders(
            DatabaseType::MySql,
            sql,
            &find_placeholders(DatabaseType::MySql, sql),
            &[],
            &[(":n".into(), "-5".into())],
        )
        .unwrap();
        assert_eq!(out, "SELECT 10 -(-5), 1 AS keep");
    }

    #[test]
    fn directives_accept_colon_names() {
        let spec = parse_spec(":since date = 2026-01-01").unwrap();
        assert_eq!(spec.name, ":since");
        assert_eq!(spec.ty, ParamType::Date);
        assert!(parse_spec(":1 int").is_err());
        assert!(parse_spec("id int").is_err());
        assert_eq!(ParamType::parse("auto").unwrap(), ParamType::Auto);
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
