//! PostgreSQL counterpart to [`crate::query::procedure`]: pure builders for Procedure
//! Mode against `CREATE [OR REPLACE] PROCEDURE|FUNCTION` scripts. Reuses the
//! shared parameter model ([`ProcParam`], [`TypeFamily`], [`ParamArg`],
//! [`ValueError`]) so the strip UI renders the same shapes; PG-only facts
//! (argument mode, routine kind, RETURNS clause) live in the wrapper types
//! here. Dialect selection is the caller's job: T-SQL connections use the
//! `procedure` module, PostgreSQL connections use this one.

use crate::query::procedure::{
    ParamArg, ProcParam, ProcParseError, TypeFamily, ValueError, decimal_literal, err,
    guid_literal, head_slice, is_compact_date, is_iso_date, is_iso_datetime, is_iso_time,
    significant, token_eq,
};
use crate::query::tokenizer::{Token, TokenKind};

/// What kind of routine the script creates. Procedures invoke via `CALL`,
/// functions via `SELECT * FROM ...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgRoutineKind {
    Procedure,
    Function,
}

impl PgRoutineKind {
    pub fn label(self) -> &'static str {
        match self {
            PgRoutineKind::Procedure => "PROCEDURE",
            PgRoutineKind::Function => "FUNCTION",
        }
    }
}

/// PostgreSQL argument mode. `IN` is the default when the script names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgParamMode {
    In,
    Out,
    InOut,
    Variadic,
}

/// One parsed parameter: the shared [`ProcParam`] model (name, type, family,
/// default) plus the PG argument mode. `param.is_output` mirrors
/// `mode ∈ {Out, InOut}` so mode-agnostic UI code keeps working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgProcParam {
    pub param: ProcParam,
    pub mode: PgParamMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgProcHeader {
    pub schema: Option<String>,
    pub name: String,
    pub kind: PgRoutineKind,
    /// Whether the script already says `CREATE OR REPLACE`.
    pub or_replace: bool,
    /// Byte range of the verb phrase (`CREATE [OR REPLACE] PROCEDURE|FUNCTION`),
    /// used by [`rewrite_deploy`] to splice the verb without touching the body.
    pub verb_start: usize,
    pub verb_end: usize,
    /// Normalized `RETURNS` clause text for functions (e.g. `setof integer`,
    /// `table(id integer)`). `None` for procedures.
    pub returns: Option<String>,
    pub params: Vec<PgProcParam>,
}

impl PgProcHeader {
    pub fn is_function(&self) -> bool {
        self.kind == PgRoutineKind::Function
    }

    /// Fully quoted invocation target. Names are stored case-folded (bare
    /// identifiers lowered, quoted identifiers verbatim), so always quoting is
    /// semantics-preserving.
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&self.name)),
            None => quote_ident(&self.name),
        }
    }

    /// Names of parameters PostgreSQL hands back as a native result row
    /// (`OUT`/`INOUT`); no sentinel SELECT machinery is needed.
    pub fn output_param_names(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|param| matches!(param.mode, PgParamMode::Out | PgParamMode::InOut))
            .map(|param| param.param.name.clone())
            .collect()
    }
}

/// Cheap head scan, safe to run on every keystroke: returns the routine kind
/// when the script starts with `CREATE [OR REPLACE] PROCEDURE|FUNCTION`,
/// ignoring leading comments and whitespace. Never parses the argument list;
/// use [`parse_routine_header`] for that.
pub fn detect_routine_script(sql: &str) -> Option<PgRoutineKind> {
    let tokens = significant(head_slice(sql));
    detect(&tokens).map(|(kind, _, _)| kind)
}

/// Full header parse. Only call after [`detect_routine_script`] matched. Verb
/// spans are byte offsets into the original script.
pub fn parse_routine_header(sql: &str) -> Result<PgProcHeader, ProcParseError> {
    let tokens = significant(head_slice(sql));
    let Some((kind, kw_index, or_replace)) = detect(&tokens) else {
        return Err(err(
            "script does not start with CREATE [OR REPLACE] PROCEDURE or FUNCTION",
        ));
    };
    let verb_start = tokens[0].start;
    let verb_end = tokens[kw_index].end;

    let (schema, name, mut index) = parse_object_name(sql, &tokens, kw_index + 1)?;

    if tokens.get(index).is_none_or(|t| t.text != "(") {
        return Err(err("expected ( after routine name"));
    }
    index += 1;

    let mut params = Vec::new();
    loop {
        let Some(token) = tokens.get(index) else {
            return Err(err("unterminated parameter list"));
        };
        if token.text == ")" {
            index += 1;
            break;
        }
        let ordinal = params.len() + 1;
        let (param, next) = parse_pg_param(sql, &tokens, index, ordinal)?;
        params.push(param);
        index = next;
        match tokens.get(index).map(|t| t.text) {
            Some(",") => index += 1,
            Some(")") => {
                index += 1;
                break;
            }
            _ => return Err(err("expected , or ) in parameter list")),
        }
    }

    let mut returns = None;
    if kind == PgRoutineKind::Function && tokens.get(index).is_some_and(|t| token_eq(*t, "RETURNS"))
    {
        let (clause, _) = capture_returns(&tokens, index + 1);
        if !clause.is_empty() {
            returns = Some(clause);
        }
    }

    Ok(PgProcHeader {
        schema,
        name,
        kind,
        or_replace,
        verb_start,
        verb_end,
        returns,
        params,
    })
}

/// Normalizes the leading verb to `CREATE OR REPLACE`, preserving the rest of
/// the script byte-for-byte. No probe/verb-flip dance is needed on PG; note
/// `CREATE OR REPLACE FUNCTION` still fails server-side when the signature
/// changed — that surfaces as a normal execution error.
pub fn rewrite_deploy(sql: &str, header: &PgProcHeader) -> String {
    let replacement = match header.kind {
        PgRoutineKind::Procedure => "CREATE OR REPLACE PROCEDURE",
        PgRoutineKind::Function => "CREATE OR REPLACE FUNCTION",
    };
    let mut out = String::with_capacity(sql.len() + 16);
    out.push_str(&sql[..header.verb_start]);
    out.push_str(replacement);
    out.push_str(&sql[header.verb_end..]);
    out
}

/// Existence probe (display only on PG — deploy always uses OR REPLACE).
/// Matches by name/schema/kind against `pg_proc`; overloads count as existing.
pub fn object_probe_sql(header: &PgProcHeader) -> String {
    let name = header.name.replace('\'', "''");
    let kind_filter = match header.kind {
        PgRoutineKind::Procedure => "p.prokind = 'p'",
        PgRoutineKind::Function => "p.prokind IN ('f', 'w')",
    };
    let schema_filter = match &header.schema {
        Some(schema) => format!("n.nspname = '{}'", schema.replace('\'', "''")),
        None => "n.nspname = ANY (pg_catalog.current_schemas(false))".to_string(),
    };
    format!(
        "SELECT CASE WHEN EXISTS (\n    SELECT 1 FROM pg_catalog.pg_proc p\n    JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace\n    WHERE p.proname = '{name}' AND {schema_filter} AND {kind_filter}\n) THEN 1 ELSE 0 END AS proc_exists;"
    )
}

pub fn rollback_begin_sql() -> &'static str {
    "BEGIN;"
}

pub fn rollback_end_sql() -> &'static str {
    "ROLLBACK;"
}

/// Auto-rollback mode composes deploy + invoke into ONE batch so they share a
/// single connection and transaction. PostgreSQL DDL is transactional and
/// `CREATE OR REPLACE` carries no only-statement-in-batch restriction, so the
/// deploy nests directly — no `sp_executesql`-style indirection.
pub fn build_rollback_batch(deploy_sql: &str, invoke_sql: &str) -> String {
    let deploy = deploy_sql.trim_end();
    let invoke = invoke_sql.trim_end();
    let mut batch = String::with_capacity(deploy.len() + invoke.len() + 32);
    batch.push_str(rollback_begin_sql());
    batch.push('\n');
    batch.push_str(deploy);
    if !deploy.ends_with(';') {
        batch.push(';');
    }
    batch.push('\n');
    batch.push_str(invoke);
    if !invoke.ends_with(';') {
        batch.push(';');
    }
    batch.push('\n');
    batch.push_str(rollback_end_sql());
    batch
}

/// Composes the invocation statement. Procedures become
/// `CALL schema.name(arg, ...)` with every argument positional: literals for
/// supplied values, `NULL` for NULL/`OUT`/unset-`INOUT` slots, and the declared
/// default expression inlined when an unsupplied defaulted parameter sits
/// before a supplied one (trailing defaulted slots are simply omitted).
/// Functions become `SELECT * FROM schema.name(args)` with `OUT` parameters
/// excluded from the argument list. PostgreSQL returns `OUT`/`INOUT` values as
/// a native result row, so no sentinel/DECLARE machinery exists here. A value
/// supplied for an `OUT` parameter is ignored (`NULL` is passed): the callee
/// never sees `OUT` inputs.
pub fn build_invoke_sql(header: &PgProcHeader, args: &[ParamArg]) -> Result<String, ValueError> {
    // (rendered text, omittable-when-trailing)
    let mut entries: Vec<(String, bool)> = Vec::new();

    for pg_param in &header.params {
        let param = &pg_param.param;
        let arg = args
            .iter()
            .find(|arg| arg.name.eq_ignore_ascii_case(&param.name));

        match pg_param.mode {
            PgParamMode::Variadic => {
                if arg.is_some_and(|a| a.value.is_some()) {
                    return Err(ValueError {
                        param: param.name.clone(),
                        message: "variadic parameters are not editable".to_string(),
                    });
                }
                // Omitted: a variadic parameter is always last and may be empty.
            }
            PgParamMode::Out => {
                if header.kind == PgRoutineKind::Procedure {
                    // PG 14+ requires OUT slots in CALL; NULL is the idiom.
                    entries.push(("NULL".to_string(), false));
                }
            }
            PgParamMode::In | PgParamMode::InOut => match arg {
                Some(arg) => match &arg.value {
                    Some(raw) => {
                        if !param.editable() {
                            return Err(ValueError {
                                param: param.name.clone(),
                                message: "values for this type are not editable".to_string(),
                            });
                        }
                        let literal = sql_literal(raw, param).map_err(|message| ValueError {
                            param: param.name.clone(),
                            message,
                        })?;
                        entries.push((literal, false));
                    }
                    None => entries.push(("NULL".to_string(), false)),
                },
                None => {
                    if let Some(default) = &param.default_expr {
                        entries.push((default.clone(), true));
                    } else if pg_param.mode == PgParamMode::InOut {
                        entries.push(("NULL".to_string(), false));
                    } else if !param.editable() {
                        return Err(ValueError {
                            param: param.name.clone(),
                            message: format!(
                                "type {} is not supported and no default is declared",
                                param.data_type
                            ),
                        });
                    } else {
                        return Err(ValueError {
                            param: param.name.clone(),
                            message: "no value supplied and no default declared".to_string(),
                        });
                    }
                }
            },
        }
    }

    while entries.last().is_some_and(|(_, omittable)| *omittable) {
        entries.pop();
    }
    let arg_list = entries
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    Ok(match header.kind {
        PgRoutineKind::Procedure => format!("CALL {}({});", header.qualified_name(), arg_list),
        PgRoutineKind::Function => {
            format!("SELECT * FROM {}({});", header.qualified_name(), arg_list)
        }
    })
}

/// Validates and encodes one user-supplied value as a PostgreSQL literal:
/// standard single-quote doubling (no `N''` prefix, no `E''` needed),
/// `true`/`false` booleans, `'\xHEX'` bytea.
pub fn sql_literal(raw: &str, param: &ProcParam) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() && param.family != TypeFamily::String {
        return Err("value is empty; check NULL to pass NULL".to_string());
    }
    match param.family {
        TypeFamily::Bit => match value.to_ascii_lowercase().as_str() {
            "1" | "t" | "true" => Ok("true".to_string()),
            "0" | "f" | "false" => Ok("false".to_string()),
            _ => Err("expected true or false".to_string()),
        },
        TypeFamily::Integer => integer_literal(value, &param.base_type),
        TypeFamily::Decimal => decimal_literal(value),
        TypeFamily::DateTime => datetime_literal(value, &param.base_type),
        TypeFamily::String | TypeFamily::Xml => Ok(quote_string(value)),
        TypeFamily::Guid => guid_literal(value),
        TypeFamily::Binary => binary_literal(value),
        TypeFamily::Unsupported => Err("values for this type are not editable".to_string()),
    }
}

/// Coarse family for a PostgreSQL base type (uppercased, sizes stripped, e.g.
/// `CHARACTER VARYING`, `TIMESTAMP WITH TIME ZONE`). Array types (trailing
/// `[]`) are unsupported in the strip, like T-SQL table-valued parameters.
pub fn type_family(base_type: &str) -> TypeFamily {
    let base = base_type.to_ascii_uppercase();
    if base.ends_with("[]") {
        return TypeFamily::Unsupported;
    }
    match base.as_str() {
        "BOOLEAN" | "BOOL" => TypeFamily::Bit,
        "SMALLINT" | "INT2" | "INTEGER" | "INT" | "INT4" | "BIGINT" | "INT8" => TypeFamily::Integer,
        "NUMERIC" | "DECIMAL" | "DEC" | "REAL" | "FLOAT4" | "DOUBLE PRECISION" | "FLOAT8"
        | "FLOAT" | "MONEY" => TypeFamily::Decimal,
        "DATE"
        | "TIME"
        | "TIMETZ"
        | "TIME WITH TIME ZONE"
        | "TIME WITHOUT TIME ZONE"
        | "TIMESTAMP"
        | "TIMESTAMPTZ"
        | "TIMESTAMP WITH TIME ZONE"
        | "TIMESTAMP WITHOUT TIME ZONE" => TypeFamily::DateTime,
        "TEXT" | "VARCHAR" | "CHARACTER VARYING" | "CHARACTER" | "CHAR" | "BPCHAR" | "NAME"
        | "CITEXT" | "INTERVAL" | "JSON" | "JSONB" => TypeFamily::String,
        "UUID" => TypeFamily::Guid,
        "BYTEA" => TypeFamily::Binary,
        "XML" => TypeFamily::Xml,
        _ => TypeFamily::Unsupported,
    }
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn integer_literal(value: &str, base_type: &str) -> Result<String, String> {
    let parsed: i128 = value
        .parse()
        .map_err(|_| "expected an integer".to_string())?;
    let (min, max): (i128, i128) = match base_type {
        "SMALLINT" | "INT2" => (i16::MIN as i128, i16::MAX as i128),
        "INTEGER" | "INT" | "INT4" => (i32::MIN as i128, i32::MAX as i128),
        "BIGINT" | "INT8" => (i64::MIN as i128, i64::MAX as i128),
        _ => (i128::MIN, i128::MAX),
    };
    if parsed < min || parsed > max {
        return Err(format!("out of range for {}", base_type.to_lowercase()));
    }
    Ok(parsed.to_string())
}

fn datetime_literal(value: &str, base_type: &str) -> Result<String, String> {
    let is_time_type =
        base_type == "TIME" || base_type == "TIMETZ" || base_type.starts_with("TIME ");
    let date_only = is_iso_date(value) || is_compact_date(value);
    let ok = if base_type == "DATE" {
        date_only
    } else if is_time_type {
        is_iso_time(value)
    } else {
        date_only || is_iso_datetime(value)
    };
    if ok {
        Ok(format!("'{value}'"))
    } else if base_type == "DATE" {
        Err("expected yyyy-mm-dd".to_string())
    } else if is_time_type {
        Err("expected hh:mm:ss".to_string())
    } else {
        Err("expected yyyy-mm-dd hh:mm:ss".to_string())
    }
}

fn binary_literal(value: &str) -> Result<String, String> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .or_else(|| value.strip_prefix("\\x"))
        .or_else(|| value.strip_prefix("\\X"));
    let Some(hex) = hex else {
        return Err("hex must start with 0x or \\x".to_string());
    };
    if hex.len() % 2 == 0 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(format!("'\\x{hex}'"))
    } else {
        Err("expected an even number of hex digits".to_string())
    }
}

/// Matches `CREATE [OR REPLACE] PROCEDURE|FUNCTION`, returning the kind, the
/// index of the PROCEDURE/FUNCTION token, and whether OR REPLACE was present.
fn detect(tokens: &[Token<'_>]) -> Option<(PgRoutineKind, usize, bool)> {
    let first = tokens.first()?;
    if !token_eq(*first, "CREATE") {
        return None;
    }
    let (kw_index, or_replace) = if token_eq(*tokens.get(1)?, "OR")
        && tokens.get(2).is_some_and(|t| token_eq(*t, "REPLACE"))
    {
        (3, true)
    } else {
        (1, false)
    };
    let keyword = tokens.get(kw_index)?;
    if token_eq(*keyword, "PROCEDURE") {
        Some((PgRoutineKind::Procedure, kw_index, or_replace))
    } else if token_eq(*keyword, "FUNCTION") {
        Some((PgRoutineKind::Function, kw_index, or_replace))
    } else {
        None
    }
}

/// Words that can begin a multi-word type name; a bare first token in this set
/// is read as (part of) an unnamed parameter's type, not as a parameter name.
/// Parameters actually named one of these must be double-quoted.
const TYPE_START_WORDS: &[&str] = &[
    "DOUBLE",
    "CHARACTER",
    "TIMESTAMP",
    "TIME",
    "BIT",
    "NATIONAL",
    "INTERVAL",
];

/// Keywords that end the RETURNS clause capture.
const RETURNS_STOP_WORDS: &[&str] = &[
    "LANGUAGE",
    "AS",
    "WINDOW",
    "IMMUTABLE",
    "STABLE",
    "VOLATILE",
    "STRICT",
    "CALLED",
    "LEAKPROOF",
    "NOT",
    "SECURITY",
    "PARALLEL",
    "COST",
    "ROWS",
    "SET",
    "SUPPORT",
    "TRANSFORM",
    "RETURN",
    "BEGIN",
];

/// A token that can be part of a bare identifier or word-form type name.
fn ident_like(token: &Token<'_>) -> bool {
    matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword) && bare_ident_text(token.text)
}

fn bare_ident_text(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
}

/// Reads one identifier (bare or double-quoted) at `index`. Bare identifiers
/// are case-folded to lowercase like PostgreSQL does; quoted identifiers keep
/// their case with `""` unescaped. Returns the name and the next token index.
///
/// The shared tokenizer emits `"` as a lone symbol (double quotes are not a
/// T-SQL string form), so quoted identifiers are re-scanned from the raw
/// bytes. Apostrophes inside quoted identifiers are not supported (the
/// tokenizer would have folded the trailing text into a string literal).
fn read_ident(sql: &str, tokens: &[Token<'_>], index: usize) -> Option<(String, usize)> {
    let token = tokens.get(index)?;
    if token.text == "\"" {
        let (name, end_byte) = read_quoted_ident(sql, token.start)?;
        let mut next = index + 1;
        while tokens.get(next).is_some_and(|t| t.start < end_byte) {
            next += 1;
        }
        return Some((name, next));
    }
    if ident_like(token) {
        return Some((token.text.to_ascii_lowercase(), index + 1));
    }
    None
}

/// Scans a `"..."` identifier from the raw script starting at the opening
/// quote, unescaping doubled quotes. Returns the content and the byte offset
/// one past the closing quote.
fn read_quoted_ident(sql: &str, start: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if bytes.get(i + 1) == Some(&b'"') {
                out.push('"');
                i += 2;
                continue;
            }
            if out.is_empty() {
                return None;
            }
            return Some((out, i + 1));
        }
        let ch = sql[i..].chars().next()?;
        out.push(ch);
        i += ch.len_utf8();
    }
    None
}

/// Parses `[schema.]name` (dotted bare or quoted identifier parts) starting at
/// `index`. Skips an optional leading database qualifier.
fn parse_object_name(
    sql: &str,
    tokens: &[Token<'_>],
    index: usize,
) -> Result<(Option<String>, String, usize), ProcParseError> {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = index;
    loop {
        let (part, next) =
            read_ident(sql, tokens, cursor).ok_or_else(|| err("expected a routine name"))?;
        parts.push(part);
        cursor = next;
        if tokens.get(cursor).is_some_and(|token| token.text == ".") {
            cursor += 1;
            continue;
        }
        break;
    }
    if parts.len() > 3 {
        return Err(err("invalid routine name"));
    }
    let name = parts.pop().unwrap();
    let schema = parts.pop();
    Ok((schema, name, cursor))
}

/// Parses one parameter starting inside the argument list:
/// `[IN|OUT|INOUT|VARIADIC] [name] type [DEFAULT expr | = expr]`. Unnamed
/// parameters get the positional name `$N`. Returns the param and the index of
/// the terminating `,` or `)`.
fn parse_pg_param(
    sql: &str,
    tokens: &[Token<'_>],
    index: usize,
    ordinal: usize,
) -> Result<(PgProcParam, usize), ProcParseError> {
    let mut cursor = index;

    let mut mode = PgParamMode::In;
    if let Some(token) = tokens.get(cursor) {
        let followed = tokens
            .get(cursor + 1)
            .is_some_and(|t| t.text != "," && t.text != ")");
        if followed {
            let parsed_mode = if token_eq(*token, "IN") {
                Some(PgParamMode::In)
            } else if token_eq(*token, "OUT") {
                Some(PgParamMode::Out)
            } else if token_eq(*token, "INOUT") {
                Some(PgParamMode::InOut)
            } else if token_eq(*token, "VARIADIC") {
                Some(PgParamMode::Variadic)
            } else {
                None
            };
            if let Some(parsed) = parsed_mode {
                mode = parsed;
                cursor += 1;
            }
        }
    }

    // Optional name: present when the first token is quoted, or a bare word
    // followed by another word/quote and not itself the start of a multi-word
    // type name (`double precision`, `timestamp with time zone`, ...).
    let mut name: Option<String> = None;
    let first = *tokens
        .get(cursor)
        .ok_or_else(|| err("expected a parameter"))?;
    if first.text == "\"" {
        let (parsed, next) =
            read_ident(sql, tokens, cursor).ok_or_else(|| err("invalid quoted identifier"))?;
        name = Some(parsed);
        cursor = next;
    } else if ident_like(&first) {
        let next_starts_type = tokens
            .get(cursor + 1)
            .is_some_and(|t| ident_like(t) || t.text == "\"");
        let first_is_type_start = TYPE_START_WORDS
            .iter()
            .any(|word| first.text.eq_ignore_ascii_case(word));
        if next_starts_type && !first_is_type_start {
            name = Some(first.text.to_ascii_lowercase());
            cursor += 1;
        }
    }

    // Type: everything up to a top-level `,`, `)`, `=`, or DEFAULT.
    let type_start = cursor;
    let mut depth = 0usize;
    while let Some(token) = tokens.get(cursor) {
        if depth == 0 {
            let text = token.text;
            if text == "," || text == ")" || text == "=" || token_eq(*token, "DEFAULT") {
                break;
            }
        }
        if token.text == "(" {
            depth += 1;
        } else if token.text == ")" {
            depth = depth.saturating_sub(1);
        }
        cursor += 1;
    }
    if cursor == type_start {
        return Err(err("expected a data type"));
    }
    let data_type = normalize_type(&tokens[type_start..cursor]);
    let base_type = base_type_of(&data_type);

    let mut default_expr = None;
    if let Some(token) = tokens.get(cursor)
        && (token.text == "=" || token_eq(*token, "DEFAULT"))
    {
        let (expr, next) = read_default(sql, tokens, cursor + 1);
        if expr.is_empty() {
            return Err(err("expected a default expression"));
        }
        default_expr = Some(expr);
        cursor = next;
    }

    let family = type_family(&base_type);
    let is_output = matches!(mode, PgParamMode::Out | PgParamMode::InOut);
    Ok((
        PgProcParam {
            param: ProcParam {
                name: name.unwrap_or_else(|| format!("${ordinal}")),
                data_type,
                base_type,
                family,
                is_output,
                is_readonly: false,
                default_expr,
            },
            mode,
        },
        cursor,
    ))
}

/// Joins the tokens of a type span into display form: single spaces between
/// words (`timestamp(3) with time zone`), none around `.`/`(`/`,`/`)` or the
/// `[]` array suffix, lowercased.
fn normalize_type(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    let mut prev_glue = false;
    for token in tokens {
        let word = matches!(
            token.kind,
            TokenKind::Identifier | TokenKind::Keyword | TokenKind::Number
        ) && !token.text.starts_with('[');
        if word && prev_glue {
            out.push(' ');
        }
        out.push_str(&token.text.to_ascii_lowercase());
        prev_glue = word || token.text == ")" || token.text.starts_with('[');
    }
    out
}

/// Uppercased base of a normalized type: size parens stripped, `[]` suffix
/// kept so arrays classify as unsupported (e.g. `timestamp(3) with time zone`
/// → `TIMESTAMP WITH TIME ZONE`, `integer[]` → `INTEGER[]`).
fn base_type_of(data_type: &str) -> String {
    let mut stripped = String::new();
    let mut depth = 0usize;
    for ch in data_type.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => stripped.push(ch),
            _ => {}
        }
    }
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// Reads a default expression from after `DEFAULT`/`=` until a top-level `,`
/// or the `)` closing the argument list, as a raw script slice.
fn read_default(sql: &str, tokens: &[Token<'_>], start: usize) -> (String, usize) {
    let mut cursor = start;
    let mut depth = 0usize;
    let mut start_byte = None;
    let mut end_byte = None;
    while let Some(token) = tokens.get(cursor) {
        if depth == 0 && (token.text == "," || token.text == ")") {
            break;
        }
        if token.text == "(" {
            depth += 1;
        } else if token.text == ")" {
            depth = depth.saturating_sub(1);
        }
        start_byte.get_or_insert(token.start);
        end_byte = Some(token.end);
        cursor += 1;
    }
    match (start_byte, end_byte) {
        (Some(start), Some(end)) => (sql[start..end].trim().to_string(), cursor),
        _ => (String::new(), cursor),
    }
}

/// Captures the RETURNS clause: `TABLE(...)`, `SETOF type`, or a plain type,
/// stopping at the first attribute keyword (`LANGUAGE`, `AS`, ...) or foreign
/// symbol. Returns the normalized clause text and the next token index.
fn capture_returns(tokens: &[Token<'_>], start: usize) -> (String, usize) {
    let mut end = start;
    let mut depth = 0usize;
    while let Some(token) = tokens.get(end) {
        let text = token.text;
        if depth == 0 {
            if text == ")" {
                break;
            }
            if matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword)
                && RETURNS_STOP_WORDS.iter().any(|word| token_eq(*token, word))
            {
                break;
            }
            if token.kind == TokenKind::Symbol && !matches!(text, "(" | ".") {
                break;
            }
        }
        if text == "(" {
            depth += 1;
        } else if text == ")" {
            depth = depth.saturating_sub(1);
        }
        end += 1;
    }
    (normalize_type(&tokens[start..end]), end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg_header(sql: &str) -> PgProcHeader {
        parse_routine_header(sql).expect("header should parse")
    }

    fn arg(name: &str, value: Option<&str>) -> ParamArg {
        ParamArg {
            name: name.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn detects_pg_create_variants() {
        assert_eq!(
            detect_routine_script("CREATE PROCEDURE public.p() LANGUAGE sql AS $$ SELECT 1 $$"),
            Some(PgRoutineKind::Procedure)
        );
        assert_eq!(
            detect_routine_script(
                "-- lead\nCREATE OR REPLACE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql"
            ),
            Some(PgRoutineKind::Function)
        );
        assert_eq!(
            detect_routine_script("/* x */ CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$"),
            Some(PgRoutineKind::Function)
        );
        assert_eq!(detect_routine_script("SELECT 1"), None);
        assert_eq!(detect_routine_script("CREATE TABLE t (a int)"), None);
        assert_eq!(
            detect_routine_script("CREATE OR REPLACE VIEW v AS SELECT 1"),
            None
        );
        assert_eq!(detect_routine_script(""), None);
    }

    #[test]
    fn parses_procedure_header_with_modes_and_defaults() {
        let header = pg_header(
            "CREATE PROCEDURE app.transfer(IN from_id integer, INOUT total numeric(10, 2), OUT status text, note text DEFAULT 'none')\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  status := 'ok';\nEND;\n$$",
        );
        assert_eq!(header.kind, PgRoutineKind::Procedure);
        assert!(!header.or_replace);
        assert_eq!(header.schema.as_deref(), Some("app"));
        assert_eq!(header.name, "transfer");
        assert_eq!(header.returns, None);
        assert_eq!(header.params.len(), 4);

        assert_eq!(header.params[0].param.name, "from_id");
        assert_eq!(header.params[0].mode, PgParamMode::In);
        assert_eq!(header.params[0].param.data_type, "integer");
        assert_eq!(header.params[0].param.family, TypeFamily::Integer);
        assert!(!header.params[0].param.is_output);

        assert_eq!(header.params[1].param.name, "total");
        assert_eq!(header.params[1].mode, PgParamMode::InOut);
        assert_eq!(header.params[1].param.data_type, "numeric(10,2)");
        assert!(header.params[1].param.is_output);

        assert_eq!(header.params[2].param.name, "status");
        assert_eq!(header.params[2].mode, PgParamMode::Out);
        assert_eq!(header.params[2].param.data_type, "text");
        assert!(header.params[2].param.is_output);

        assert_eq!(header.params[3].param.name, "note");
        assert_eq!(
            header.params[3].param.default_expr.as_deref(),
            Some("'none'")
        );

        assert_eq!(header.output_param_names(), vec!["total", "status"]);
        assert_eq!(header.qualified_name(), "\"app\".\"transfer\"");
    }

    #[test]
    fn parses_function_header_returns_and_language() {
        let header = pg_header(
            "CREATE OR REPLACE FUNCTION public.add_tax(amount numeric, rate numeric DEFAULT 0.075) RETURNS numeric LANGUAGE sql AS $fn$ SELECT amount * (1 + rate); $fn$",
        );
        assert_eq!(header.kind, PgRoutineKind::Function);
        assert!(header.is_function());
        assert!(header.or_replace);
        assert_eq!(header.schema.as_deref(), Some("public"));
        assert_eq!(header.name, "add_tax");
        assert_eq!(header.returns.as_deref(), Some("numeric"));
        assert_eq!(header.params.len(), 2);
        assert_eq!(header.params[0].param.name, "amount");
        assert_eq!(
            header.params[1].param.default_expr.as_deref(),
            Some("0.075")
        );

        let setof =
            pg_header("CREATE FUNCTION f() RETURNS SETOF integer LANGUAGE sql AS $$ SELECT 1 $$");
        assert_eq!(setof.returns.as_deref(), Some("setof integer"));
        let table = pg_header(
            "CREATE FUNCTION g() RETURNS TABLE(id integer, name text) AS $$ SELECT 1, 'x' $$ LANGUAGE sql",
        );
        assert_eq!(
            table.returns.as_deref(),
            Some("table(id integer,name text)")
        );
    }

    #[test]
    fn parses_dollar_quoted_bodies() {
        let header = pg_header(
            "CREATE PROCEDURE p(a integer)\nLANGUAGE plpgsql\nAS $body$\nBEGIN\n  INSERT INTO t VALUES (a, 'x,y');\nEND;\n$body$",
        );
        assert_eq!(header.name, "p");
        assert_eq!(header.params.len(), 1);
        assert_eq!(header.params[0].param.name, "a");
    }

    #[test]
    fn parses_quoted_and_folded_identifiers() {
        let header = pg_header(
            "CREATE PROCEDURE \"MySchema\".\"Do It\"(\"Id\" integer, \"A\"\"B\" text) LANGUAGE sql AS $$ SELECT 1 $$",
        );
        assert_eq!(header.schema.as_deref(), Some("MySchema"));
        assert_eq!(header.name, "Do It");
        assert_eq!(header.params[0].param.name, "Id");
        assert_eq!(header.params[1].param.name, "A\"B");
        assert_eq!(header.qualified_name(), "\"MySchema\".\"Do It\"");

        let folded = pg_header(
            "CREATE FUNCTION Public.GetX() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
        );
        assert_eq!(folded.schema.as_deref(), Some("public"));
        assert_eq!(folded.name, "getx");
        assert_eq!(folded.qualified_name(), "\"public\".\"getx\"");
    }

    #[test]
    fn parses_unnamed_params() {
        let header = pg_header(
            "CREATE FUNCTION public.g(integer, text) RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
        );
        assert_eq!(header.params.len(), 2);
        assert_eq!(header.params[0].param.name, "$1");
        assert_eq!(header.params[0].param.data_type, "integer");
        assert_eq!(header.params[1].param.name, "$2");
        assert_eq!(header.params[1].param.data_type, "text");
    }

    #[test]
    fn parses_multiword_and_array_types() {
        let header = pg_header(
            "CREATE FUNCTION public.f(a double precision, b character varying(20), c timestamp with time zone, d integer[], VARIADIC rest text[]) RETURNS void AS $$ SELECT 1 $$ LANGUAGE sql",
        );
        assert_eq!(header.params[0].param.data_type, "double precision");
        assert_eq!(header.params[0].param.family, TypeFamily::Decimal);
        assert_eq!(header.params[1].param.data_type, "character varying(20)");
        assert_eq!(header.params[1].param.base_type, "CHARACTER VARYING");
        assert_eq!(header.params[1].param.family, TypeFamily::String);
        assert_eq!(header.params[2].param.data_type, "timestamp with time zone");
        assert_eq!(header.params[2].param.family, TypeFamily::DateTime);
        assert_eq!(header.params[3].param.data_type, "integer[]");
        assert_eq!(header.params[3].param.family, TypeFamily::Unsupported);
        assert!(!header.params[3].param.editable());
        assert_eq!(header.params[4].mode, PgParamMode::Variadic);
        assert_eq!(header.params[4].param.data_type, "text[]");

        // Unnamed multi-word type: the whole span is the type, not a name.
        let unnamed = pg_header(
            "CREATE FUNCTION h(double precision) RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
        );
        assert_eq!(unnamed.params[0].param.name, "$1");
        assert_eq!(unnamed.params[0].param.data_type, "double precision");
    }

    #[test]
    fn parses_defaults_with_equals_and_calls() {
        let header = pg_header(
            "CREATE PROCEDURE p(a integer = greatest(1, 2), b date DEFAULT '2024-01-01') LANGUAGE sql AS $$ SELECT 1 $$",
        );
        assert_eq!(
            header.params[0].param.default_expr.as_deref(),
            Some("greatest(1, 2)")
        );
        assert_eq!(
            header.params[1].param.default_expr.as_deref(),
            Some("'2024-01-01'")
        );
    }

    #[test]
    fn pg_literal_validation() {
        let int_param = ProcParam {
            name: "n".into(),
            data_type: "integer".into(),
            base_type: "INTEGER".into(),
            family: TypeFamily::Integer,
            is_output: false,
            is_readonly: false,
            default_expr: None,
        };
        assert_eq!(sql_literal("42", &int_param).unwrap(), "42");
        assert!(sql_literal("abc", &int_param).is_err());
        assert!(sql_literal("99999999999", &int_param).is_err());

        let small = ProcParam {
            base_type: "SMALLINT".into(),
            ..int_param.clone()
        };
        assert!(sql_literal("40000", &small).is_err());

        let boolean = ProcParam {
            base_type: "BOOLEAN".into(),
            family: TypeFamily::Bit,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("true", &boolean).unwrap(), "true");
        assert_eq!(sql_literal("0", &boolean).unwrap(), "false");
        assert!(sql_literal("yes", &boolean).is_err());

        let text = ProcParam {
            base_type: "TEXT".into(),
            family: TypeFamily::String,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("o'clock", &text).unwrap(), "'o''clock'");

        let date = ProcParam {
            base_type: "DATE".into(),
            family: TypeFamily::DateTime,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("2024-02-29", &date).unwrap(), "'2024-02-29'");
        assert!(sql_literal("2023-02-29", &date).is_err());

        let timestamptz = ProcParam {
            base_type: "TIMESTAMP WITH TIME ZONE".into(),
            family: TypeFamily::DateTime,
            ..int_param.clone()
        };
        assert_eq!(
            sql_literal("2024-01-01 10:00:00", &timestamptz).unwrap(),
            "'2024-01-01 10:00:00'"
        );
        assert!(sql_literal("10:00:00", &timestamptz).is_err());

        let time = ProcParam {
            base_type: "TIME".into(),
            family: TypeFamily::DateTime,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("10:30:00", &time).unwrap(), "'10:30:00'");
        assert!(sql_literal("2024-01-01", &time).is_err());

        let numeric = ProcParam {
            base_type: "NUMERIC".into(),
            family: TypeFamily::Decimal,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("1.5", &numeric).unwrap(), "1.5");
        assert!(sql_literal("x", &numeric).is_err());

        let uuid = ProcParam {
            base_type: "UUID".into(),
            family: TypeFamily::Guid,
            ..int_param.clone()
        };
        assert!(sql_literal("00000000-0000-0000-0000-000000000000", &uuid).is_ok());
        assert!(sql_literal("not-a-uuid", &uuid).is_err());

        let bytea = ProcParam {
            base_type: "BYTEA".into(),
            family: TypeFamily::Binary,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("0xDEADbeef", &bytea).unwrap(), "'\\xDEADbeef'");
        assert_eq!(sql_literal("\\xdead", &bytea).unwrap(), "'\\xdead'");
        assert!(sql_literal("DEAD", &bytea).is_err());
        assert!(sql_literal("0xABC", &bytea).is_err());
    }

    #[test]
    fn invoke_call_covers_null_out_and_inout() {
        let header = pg_header(
            "CREATE PROCEDURE app.transfer(IN from_id integer, INOUT total numeric, OUT status text, note text DEFAULT 'none') LANGUAGE sql AS $$ SELECT 1 $$",
        );
        let sql = build_invoke_sql(&header, &[arg("from_id", Some("7"))]).unwrap();
        // INOUT without a value and OUT both pass NULL; the trailing defaulted
        // parameter is omitted entirely.
        assert_eq!(sql, "CALL \"app\".\"transfer\"(7, NULL, NULL);");

        let with_values = build_invoke_sql(
            &header,
            &[
                arg("from_id", Some("7")),
                arg("total", Some("1.25")),
                arg("note", Some("it's fine")),
            ],
        )
        .unwrap();
        assert_eq!(
            with_values,
            "CALL \"app\".\"transfer\"(7, 1.25, NULL, 'it''s fine');"
        );

        let null_checked =
            build_invoke_sql(&header, &[arg("from_id", None), arg("total", None)]).unwrap();
        assert_eq!(null_checked, "CALL \"app\".\"transfer\"(NULL, NULL, NULL);");
    }

    #[test]
    fn invoke_inlines_mid_defaults_and_trims_trailing() {
        let header = pg_header(
            "CREATE PROCEDURE public.log(a integer, b text DEFAULT 'x', c integer) LANGUAGE sql AS $$ SELECT 1 $$",
        );
        // The unsupplied default sits before a supplied value, so its declared
        // expression is inlined positionally.
        let sql = build_invoke_sql(&header, &[arg("a", Some("1")), arg("c", Some("2"))]).unwrap();
        assert_eq!(sql, "CALL \"public\".\"log\"(1, 'x', 2);");

        let trailing = pg_header(
            "CREATE PROCEDURE public.p(a integer, note text DEFAULT 'none') LANGUAGE sql AS $$ SELECT 1 $$",
        );
        let sql = build_invoke_sql(&trailing, &[arg("a", Some("1"))]).unwrap();
        assert_eq!(sql, "CALL \"public\".\"p\"(1);");

        let empty = pg_header("CREATE PROCEDURE public.noop() LANGUAGE sql AS $$ SELECT 1 $$");
        assert_eq!(
            build_invoke_sql(&empty, &[]).unwrap(),
            "CALL \"public\".\"noop\"();"
        );
    }

    #[test]
    fn invoke_function_uses_select_and_skips_out() {
        let header = pg_header(
            "CREATE FUNCTION public.calc(a integer, OUT doubled integer, b boolean DEFAULT true) RETURNS record LANGUAGE sql AS $$ SELECT a * 2 $$",
        );
        let sql = build_invoke_sql(&header, &[arg("a", Some("5"))]).unwrap();
        assert_eq!(sql, "SELECT * FROM \"public\".\"calc\"(5);");

        let sql =
            build_invoke_sql(&header, &[arg("a", Some("5")), arg("b", Some("false"))]).unwrap();
        assert_eq!(sql, "SELECT * FROM \"public\".\"calc\"(5, false);");
    }

    #[test]
    fn invoke_requires_values_without_defaults() {
        let header =
            pg_header("CREATE PROCEDURE public.p(a integer) LANGUAGE sql AS $$ SELECT 1 $$");
        let err = build_invoke_sql(&header, &[]).unwrap_err();
        assert_eq!(err.param, "a");
    }

    #[test]
    fn invoke_rejects_array_and_variadic_values() {
        let header = pg_header(
            "CREATE PROCEDURE public.p(ids integer[], VARIADIC rest text[]) LANGUAGE sql AS $$ SELECT 1 $$",
        );
        let err = build_invoke_sql(&header, &[arg("ids", Some("{1}"))]).unwrap_err();
        assert_eq!(err.param, "ids");
        let err =
            build_invoke_sql(&header, &[arg("ids", None), arg("rest", Some("x"))]).unwrap_err();
        assert_eq!(err.param, "rest");
    }

    #[test]
    fn rewrite_normalizes_to_create_or_replace() {
        let sql = "-- note\nCREATE PROCEDURE public.p(a integer)\nLANGUAGE sql\nAS $$ SELECT 1 $$";
        let header = pg_header(sql);
        let rewritten = rewrite_deploy(sql, &header);
        assert_eq!(
            rewritten,
            "-- note\nCREATE OR REPLACE PROCEDURE public.p(a integer)\nLANGUAGE sql\nAS $$ SELECT 1 $$"
        );
        // Already-normalized scripts round-trip unchanged.
        let reparsed = pg_header(&rewritten);
        assert!(reparsed.or_replace);
        assert_eq!(rewrite_deploy(&rewritten, &reparsed), rewritten);

        let func = "CREATE FUNCTION f() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql";
        let func_header = pg_header(func);
        assert!(rewrite_deploy(func, &func_header).starts_with("CREATE OR REPLACE FUNCTION f()"));
    }

    #[test]
    fn probe_sql_filters_kind_and_schema() {
        let header =
            pg_header("CREATE PROCEDURE app.transfer(a integer) LANGUAGE sql AS $$ SELECT 1 $$");
        let probe = object_probe_sql(&header);
        assert!(probe.contains("p.proname = 'transfer'"));
        assert!(probe.contains("n.nspname = 'app'"));
        assert!(probe.contains("p.prokind = 'p'"));
        assert!(probe.contains("AS proc_exists;"));

        let func = pg_header("CREATE FUNCTION f1() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql");
        let probe = object_probe_sql(&func);
        assert!(probe.contains("n.nspname = ANY (pg_catalog.current_schemas(false))"));
        assert!(probe.contains("p.prokind IN ('f', 'w')"));

        let quirky = PgProcHeader {
            schema: Some("we'ird".into()),
            name: "it's".into(),
            kind: PgRoutineKind::Procedure,
            or_replace: false,
            verb_start: 0,
            verb_end: 0,
            returns: None,
            params: Vec::new(),
        };
        let probe = object_probe_sql(&quirky);
        assert!(probe.contains("p.proname = 'it''s'"));
        assert!(probe.contains("n.nspname = 'we''ird'"));
    }

    #[test]
    fn rollback_batch_wraps_transaction() {
        let batch = build_rollback_batch(
            "CREATE OR REPLACE PROCEDURE public.p() LANGUAGE sql AS $$ SELECT 'x' $$",
            "CALL \"public\".\"p\"();",
        );
        assert_eq!(
            batch,
            "BEGIN;\nCREATE OR REPLACE PROCEDURE public.p() LANGUAGE sql AS $$ SELECT 'x' $$;\nCALL \"public\".\"p\"();\nROLLBACK;"
        );
    }

    #[test]
    fn parse_errors_are_clear() {
        assert!(parse_routine_header("SELECT 1").is_err());
        assert!(parse_routine_header("CREATE PROCEDURE AS SELECT 1").is_err());
        assert!(parse_routine_header("CREATE PROCEDURE p LANGUAGE sql").is_err());
        assert!(parse_routine_header("CREATE PROCEDURE p(a integer").is_err());
        assert!(parse_routine_header("CREATE PROCEDURE p(a integer DEFAULT )").is_err());
    }
}
