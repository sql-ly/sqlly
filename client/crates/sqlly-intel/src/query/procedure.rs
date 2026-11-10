use crate::query::tokenizer::{Token, TokenKind, tokenize};

/// Leading verb of a procedure script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcVerb {
    Create,
    Alter,
    /// `CREATE OR ALTER`: deploys correctly whether or not the object exists.
    CreateOrAlter,
}

impl ProcVerb {
    pub fn label(self) -> &'static str {
        match self {
            ProcVerb::Create => "CREATE",
            ProcVerb::Alter => "ALTER",
            ProcVerb::CreateOrAlter => "CREATE OR ALTER",
        }
    }
}

/// Coarse family a parameter's declared type falls into. Drives which editor
/// widget renders the value and how the value is encoded as a T-SQL literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFamily {
    Bit,
    Integer,
    Decimal,
    DateTime,
    String,
    Guid,
    Binary,
    Xml,
    /// Table-valued params, cursors, variants, spatial types: parsed but not
    /// editable in the strip.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParam {
    /// Parameter name without the leading `@`, original case.
    pub name: String,
    /// Display form of the declared type, lowercase, e.g. `nvarchar(50)`.
    pub data_type: String,
    /// Base type keyword uppercased, e.g. `NVARCHAR`. Drives literal encoding.
    pub base_type: String,
    pub family: TypeFamily,
    pub is_output: bool,
    /// Table-valued parameters are marked READONLY in the header.
    pub is_readonly: bool,
    /// Raw text of the declared default expression, if any.
    pub default_expr: Option<String>,
}

impl ProcParam {
    pub fn has_default(&self) -> bool {
        self.default_expr.is_some()
    }

    pub fn editable(&self) -> bool {
        self.family != TypeFamily::Unsupported && !self.is_readonly
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcHeader {
    pub schema: Option<String>,
    pub name: String,
    pub verb: ProcVerb,
    /// Byte range of the leading verb phrase (`CREATE [OR ALTER] PROCEDURE`),
    /// used by [`rewrite_deploy`] to splice the verb without touching the body.
    pub verb_start: usize,
    pub verb_end: usize,
    pub params: Vec<ProcParam>,
}

impl ProcHeader {
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&self.name)),
            None => quote_ident(&self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParseError {
    pub message: String,
}

impl std::fmt::Display for ProcParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A value supplied for one parameter in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamArg {
    /// Parameter name without `@` (case-insensitive match).
    pub name: String,
    /// `None` means the NULL checkbox is on.
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    pub param: String,
    pub message: String,
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}: {}", self.param, self.message)
    }
}

/// Detection and header parsing only ever look at the head of the script.
/// Capping the lexed prefix keeps per-keystroke detection cheap on huge
/// buffers; a header buried under more than this much leading comment is
/// treated as not-a-procedure-script.
const HEAD_SCAN_CAP: usize = 64 * 1024;

pub(crate) fn head_slice(sql: &str) -> &str {
    if sql.len() <= HEAD_SCAN_CAP {
        return sql;
    }
    let mut end = HEAD_SCAN_CAP;
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    &sql[..end]
}

/// Cheap head scan, safe to run on every keystroke: returns the leading verb
/// when the script starts with `CREATE [OR ALTER] PROC[EDURE]` or
/// `ALTER PROC[EDURE]`, ignoring leading comments and whitespace. Never parses
/// the parameter list; use [`parse_proc_header`] for that.
pub fn detect_proc_script(sql: &str) -> Option<ProcVerb> {
    let tokens = significant(head_slice(sql));
    detect_verb(&tokens).map(|(verb, _)| verb)
}

/// Full header parse. Only call after [`detect_proc_script`] matched. Verb
/// spans are byte offsets into the original script (the capped scan is a
/// prefix of it, so offsets stay valid).
pub fn parse_proc_header(sql: &str) -> Result<ProcHeader, ProcParseError> {
    let tokens = significant(head_slice(sql));
    let Some((verb, proc_index)) = detect_verb(&tokens) else {
        return Err(err("script does not start with CREATE or ALTER PROCEDURE"));
    };
    let verb_start = tokens[0].start;
    let verb_end = tokens[proc_index].end;

    let mut index = proc_index + 1;
    let (schema, name, next) = parse_object_name(&tokens, index)?;
    index = next;

    let mut params = Vec::new();
    let mut depth = 0usize;
    let mut paren_list = false;
    if index < tokens.len() && tokens[index].text == "(" {
        paren_list = true;
        depth = 1;
        index += 1;
    }

    while index < tokens.len() {
        let token = tokens[index];
        if paren_list && depth == 1 && token.text == ")" {
            break;
        }
        if !paren_list && token_eq(token, "AS") {
            break;
        }
        if is_param_token(token.text) {
            let (param, next) = parse_param(sql, &tokens, index, paren_list)?;
            params.push(param);
            index = next;
            continue;
        }
        if token.text == "(" {
            depth += 1;
        } else if token.text == ")" {
            depth = depth.saturating_sub(1);
        }
        index += 1;
    }

    if !paren_list && params.is_empty() && !tokens.get(index).is_some_and(|t| token_eq(*t, "AS")) {
        // Bare form with no params must still reach AS.
        return Err(err("expected AS after procedure name"));
    }

    Ok(ProcHeader {
        schema,
        name,
        verb,
        verb_start,
        verb_end,
        params,
    })
}

/// Rewrites the leading verb to the target form, preserving the rest of the
/// script byte-for-byte. `CreateOrAlter` never needs a rewrite (callers skip).
pub fn rewrite_deploy(sql: &str, header: &ProcHeader, target: ProcVerb) -> String {
    let replacement = match target {
        ProcVerb::Create => "CREATE PROCEDURE",
        ProcVerb::Alter => "ALTER PROCEDURE",
        ProcVerb::CreateOrAlter => "CREATE OR ALTER PROCEDURE",
    };
    let mut out = String::with_capacity(sql.len() + 16);
    out.push_str(&sql[..header.verb_start]);
    out.push_str(replacement);
    out.push_str(&sql[header.verb_end..]);
    out
}

/// Existence probe run before deploy so the verb can be flipped when the
/// script and the server disagree.
pub fn object_probe_sql(header: &ProcHeader) -> String {
    let qualified = match &header.schema {
        Some(schema) => format!("{}.{}", schema, header.name),
        None => header.name.clone(),
    };
    format!(
        "SELECT CASE WHEN OBJECT_ID(N'{}', N'P') IS NULL THEN 0 ELSE 1 END AS [proc_exists];",
        qualified.replace('\'', "''")
    )
}

pub fn rollback_begin_sql() -> &'static str {
    "SET XACT_ABORT ON;\nIF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;\nBEGIN TRANSACTION;"
}

pub fn rollback_end_sql() -> &'static str {
    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;"
}

/// Auto-rollback mode composes deploy + invoke into ONE batch so they share a
/// single connection and transaction (the app's executor opens a fresh
/// connection per request, so a transaction cannot span requests). The deploy
/// runs via `sp_executesql` because `CREATE`/`ALTER PROCEDURE` must be the
/// only statement in its batch; the dynamic call re-enters batch scope, and
/// DDL is transactional on SQL Server, so the trailing ROLLBACK undoes it.
/// The invoke batch's sentinel SELECT stays the final result set; ROLLBACK
/// emits nothing.
pub fn build_rollback_batch(deploy_sql: &str, invoke_sql: &str) -> String {
    let mut batch = String::with_capacity(deploy_sql.len() + invoke_sql.len() + 160);
    batch.push_str(rollback_begin_sql());
    batch.push('\n');
    batch.push_str("EXEC sys.sp_executesql N'");
    batch.push_str(&deploy_sql.replace('\'', "''"));
    batch.push_str("';\n");
    batch.push_str(invoke_sql);
    batch.push('\n');
    batch.push_str(rollback_end_sql());
    batch
}

/// Composes the invocation batch: DECLAREs for supplied values and output
/// captures, the EXEC with named arguments, and a sentinel SELECT carrying the
/// return code (first column `__sqlly_rc`) followed by one column per OUTPUT
/// parameter, aliased to the parameter name.
pub fn build_invoke_batch(header: &ProcHeader, args: &[ParamArg]) -> Result<String, ValueError> {
    let mut declares: Vec<String> = Vec::new();
    let mut exec_args: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();

    for param in &header.params {
        let arg = args
            .iter()
            .find(|arg| arg.name.eq_ignore_ascii_case(&param.name));
        let local = local_var(&param.name);

        if !param.editable() {
            if arg.is_some_and(|a| a.value.is_some()) {
                return Err(ValueError {
                    param: param.name.clone(),
                    message: "values for this type are not editable".to_string(),
                });
            }
            if param.has_default() || param.is_readonly {
                // Omitted from the EXEC: the server applies the declared
                // default, or an empty table for a table-valued parameter.
                continue;
            }
            return Err(ValueError {
                param: param.name.clone(),
                message: format!(
                    "type {} is not supported and no default is declared",
                    param.data_type
                ),
            });
        }

        match arg {
            None => {
                if param.is_output {
                    declares.push(format!("DECLARE {local} {};", param.data_type));
                    exec_args.push(format!("{local} = {local} OUTPUT"));
                    outputs.push(param.name.clone());
                } else if param.has_default() {
                    continue;
                } else {
                    return Err(ValueError {
                        param: param.name.clone(),
                        message: "no value supplied and no default declared".to_string(),
                    });
                }
            }
            Some(arg) => {
                declares.push(match &arg.value {
                    Some(raw) => {
                        let literal = sql_literal(raw, param).map_err(|message| ValueError {
                            param: param.name.clone(),
                            message,
                        })?;
                        format!("DECLARE {local} {} = {literal};", param.data_type)
                    }
                    None => format!("DECLARE {local} {};", param.data_type),
                });
                if param.is_output {
                    exec_args.push(format!("{local} = {local} OUTPUT"));
                    outputs.push(param.name.clone());
                } else {
                    exec_args.push(format!("{local} = {local}"));
                }
            }
        }
    }

    let mut batch = String::new();
    batch.push_str("SET NOCOUNT ON;\n");
    batch.push_str("DECLARE @__sqlly_rc INT;\n");
    for declare in &declares {
        batch.push_str(declare);
        batch.push('\n');
    }
    batch.push_str(&format!("EXEC @__sqlly_rc = {}", header.qualified_name()));
    if !exec_args.is_empty() {
        batch.push(' ');
        batch.push_str(&exec_args.join(", "));
    }
    batch.push_str(";\n");
    batch.push_str("SELECT @__sqlly_rc AS [__sqlly_rc]");
    for output in &outputs {
        batch.push_str(&format!(
            ", {} AS {}",
            local_var(output),
            quote_ident(output)
        ));
    }
    batch.push(';');
    Ok(batch)
}

/// Local capture variable for a parameter, e.g. `[@name]`.
fn local_var(name: &str) -> String {
    format!("[@{}]", name.replace(']', "]]"))
}

/// Column name marking the sentinel result set the app peels off the invoke
/// batch's results.
pub const SENTINEL_COLUMN: &str = "__sqlly_rc";

/// Validates and encodes one user-supplied value as a T-SQL literal.
pub fn sql_literal(raw: &str, param: &ProcParam) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() && param.family != TypeFamily::String {
        return Err("value is empty; check NULL to pass NULL".to_string());
    }
    match param.family {
        TypeFamily::Bit => match value.to_ascii_lowercase().as_str() {
            "1" | "true" => Ok("1".to_string()),
            "0" | "false" => Ok("0".to_string()),
            _ => Err("expected 1 or 0".to_string()),
        },
        TypeFamily::Integer => integer_literal(value, &param.base_type),
        TypeFamily::Decimal => decimal_literal(value),
        TypeFamily::DateTime => datetime_literal(value, &param.base_type),
        TypeFamily::String => {
            let escaped = value.replace('\'', "''");
            if param.base_type.starts_with('N') && param.base_type != "NUMERIC" {
                Ok(format!("N'{escaped}'"))
            } else {
                Ok(format!("'{escaped}'"))
            }
        }
        TypeFamily::Guid => guid_literal(value),
        TypeFamily::Binary => binary_literal(value),
        TypeFamily::Xml => Ok(format!("'{}'", value.replace('\'', "''"))),
        TypeFamily::Unsupported => Err("values for this type are not editable".to_string()),
    }
}

fn integer_literal(value: &str, base_type: &str) -> Result<String, String> {
    let parsed: i128 = value
        .parse()
        .map_err(|_| "expected an integer".to_string())?;
    let (min, max): (i128, i128) = match base_type {
        "TINYINT" => (0, 255),
        "SMALLINT" => (-32768, 32767),
        "INT" => (i32::MIN as i128, i32::MAX as i128),
        "BIGINT" => (i64::MIN as i128, i64::MAX as i128),
        _ => (i128::MIN, i128::MAX),
    };
    if parsed < min || parsed > max {
        return Err(format!("out of range for {}", base_type.to_lowercase()));
    }
    Ok(parsed.to_string())
}

pub(crate) fn decimal_literal(value: &str) -> Result<String, String> {
    let valid = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
        && value.parse::<f64>().is_ok();
    if valid {
        Ok(value.to_string())
    } else {
        Err("expected a number".to_string())
    }
}

fn datetime_literal(value: &str, base_type: &str) -> Result<String, String> {
    let date_only = is_iso_date(value) || is_compact_date(value);
    let with_time = is_iso_datetime(value);
    let ok = if base_type == "DATE" {
        date_only
    } else if base_type == "TIME" {
        is_iso_time(value)
    } else {
        date_only || with_time
    };
    if ok {
        Ok(format!("'{value}'"))
    } else if base_type == "DATE" {
        Err("expected yyyy-mm-dd".to_string())
    } else if base_type == "TIME" {
        Err("expected hh:mm:ss".to_string())
    } else {
        Err("expected yyyy-mm-dd hh:mm:ss".to_string())
    }
}

pub(crate) fn is_iso_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        parts[0].parse::<i32>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
    ) else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

pub(crate) fn is_compact_date(value: &str) -> bool {
    value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) && {
        let month: u32 = value[4..6].parse().unwrap_or(0);
        let day: u32 = value[6..8].parse().unwrap_or(0);
        let year: i32 = value[..4].parse().unwrap_or(0);
        (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
    }
}

pub(crate) fn is_iso_time(value: &str) -> bool {
    let (clock, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !fraction.is_empty() && (fraction.len() > 7 || !fraction.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    if value.contains('.') && fraction.is_empty() {
        return false;
    }
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return false;
    }
    let hour: u32 = match parts[0].parse() {
        Ok(h) if parts[0].len() == 2 => h,
        _ => return false,
    };
    let minute: u32 = match parts[1].parse() {
        Ok(m) if parts[1].len() == 2 => m,
        _ => return false,
    };
    let second: u32 = if parts.len() == 3 {
        match parts[2].parse() {
            Ok(s) if parts[2].len() == 2 => s,
            _ => return false,
        }
    } else {
        0
    };
    hour < 24 && minute < 60 && second < 60
}

pub(crate) fn is_iso_datetime(value: &str) -> bool {
    let (date, time) = value
        .split_once('T')
        .or_else(|| value.split_once(' '))
        .unwrap_or((value, ""));
    if !is_iso_date(date) {
        return false;
    }
    let time = time.strip_suffix('Z').unwrap_or(time);
    let (clock, offset) = match time.find(['+', '-']) {
        Some(idx) if idx > 0 => (&time[..idx], Some(&time[idx..])),
        _ => (time, None),
    };
    if !is_iso_time(clock) {
        return false;
    }
    if let Some(offset) = offset {
        let offset = &offset[1..];
        if !is_iso_time(offset) {
            return false;
        }
    }
    true
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

pub(crate) fn guid_literal(value: &str) -> Result<String, String> {
    let trimmed = value
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .unwrap_or(value);
    let segments: Vec<&str> = trimmed.split('-').collect();
    let valid = segments.len() == 5
        && segments
            .iter()
            .zip([8usize, 4, 4, 4, 12])
            .all(|(segment, len)| {
                segment.len() == len && segment.chars().all(|c| c.is_ascii_hexdigit())
            });
    if valid {
        Ok(format!("'{trimmed}'"))
    } else {
        Err("invalid GUID".to_string())
    }
}

fn binary_literal(value: &str) -> Result<String, String> {
    let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    else {
        return Err("hex must start with 0x".to_string());
    };
    if hex.len() % 2 == 0 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(format!("0x{hex}"))
    } else {
        Err("expected an even number of hex digits".to_string())
    }
}

pub fn type_family(base_type: &str) -> TypeFamily {
    match base_type.to_ascii_uppercase().as_str() {
        "BIT" => TypeFamily::Bit,
        "TINYINT" | "SMALLINT" | "INT" | "BIGINT" => TypeFamily::Integer,
        "DECIMAL" | "NUMERIC" | "MONEY" | "SMALLMONEY" | "FLOAT" | "REAL" => TypeFamily::Decimal,
        "DATE" | "TIME" | "DATETIME" | "DATETIME2" | "SMALLDATETIME" | "DATETIMEOFFSET" => {
            TypeFamily::DateTime
        }
        "CHAR" | "VARCHAR" | "TEXT" | "NCHAR" | "NVARCHAR" | "NTEXT" | "SYSNAME" => {
            TypeFamily::String
        }
        "UNIQUEIDENTIFIER" => TypeFamily::Guid,
        "BINARY" | "VARBINARY" | "IMAGE" | "ROWVERSION" | "TIMESTAMP" => TypeFamily::Binary,
        "XML" => TypeFamily::Xml,
        _ => TypeFamily::Unsupported,
    }
}

fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

pub(crate) fn err(message: &str) -> ProcParseError {
    ProcParseError {
        message: message.to_string(),
    }
}

pub(crate) fn significant(sql: &str) -> Vec<Token<'_>> {
    tokenize(sql)
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect()
}

/// Matches `CREATE [OR ALTER] PROC|PROCEDURE` / `ALTER PROC|PROCEDURE`,
/// returning the verb and the index of the PROC token.
fn detect_verb(tokens: &[Token<'_>]) -> Option<(ProcVerb, usize)> {
    let first = tokens.first()?;
    let is_proc = |token: &Token<'_>| token_eq(*token, "PROC") || token_eq(*token, "PROCEDURE");
    if token_eq(*first, "ALTER") {
        return tokens
            .get(1)
            .filter(|token| is_proc(token))
            .map(|_| (ProcVerb::Alter, 1));
    }
    if !token_eq(*first, "CREATE") {
        return None;
    }
    if tokens.get(1).is_some_and(is_proc) {
        return Some((ProcVerb::Create, 1));
    }
    if token_eq(*tokens.get(1)?, "OR")
        && token_eq(*tokens.get(2)?, "ALTER")
        && tokens.get(3).is_some_and(is_proc)
    {
        return Some((ProcVerb::CreateOrAlter, 3));
    }
    None
}

/// Parses `[schema.][name]` (one or two bracketed or bare identifier parts)
/// starting at `index`. Skips an optional database qualifier.
fn parse_object_name(
    tokens: &[Token<'_>],
    index: usize,
) -> Result<(Option<String>, String, usize), ProcParseError> {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = index;
    loop {
        let Some(token) = tokens.get(cursor) else {
            return Err(err("expected a procedure name"));
        };
        let part = unquote_ident(token.text).ok_or_else(|| err("expected a procedure name"))?;
        parts.push(part);
        cursor += 1;
        if tokens.get(cursor).is_some_and(|token| token.text == ".") {
            cursor += 1;
            continue;
        }
        break;
    }
    if parts.is_empty() || parts.len() > 3 {
        return Err(err("invalid procedure name"));
    }
    let name = parts.pop().unwrap();
    let schema = parts.pop();
    Ok((schema, name, cursor))
}

fn unquote_ident(text: &str) -> Option<String> {
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return Some(inner.replace("]]", "]"));
    }
    if text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
        return Some(text[1..text.len() - 1].replace("\"\"", "\""));
    }
    // Temp procedures arrive as one `#name` / `##name` identifier token.
    let body = text
        .strip_prefix("##")
        .or_else(|| text.strip_prefix('#'))
        .unwrap_or(text);
    let valid = !body.is_empty()
        && !text.starts_with('@')
        && body
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'));
    valid.then(|| text.to_string())
}

/// Parses one parameter starting at the `@name` token: name, type with
/// optional `(n | max | prec, scale)`, optional `= default`, optional
/// OUT/OUTPUT/READONLY. Returns the param and the next index to scan.
fn parse_param(
    sql: &str,
    tokens: &[Token<'_>],
    index: usize,
    paren_list: bool,
) -> Result<(ProcParam, usize), ProcParseError> {
    let name = tokens[index].text.trim_start_matches('@').to_string();
    let mut cursor = index + 1;

    // Type: one or two identifier tokens (e.g. `nvarchar`, or a bracketed /
    // schema-qualified UDT name), plus an optional parenthesized size.
    let mut type_parts: Vec<String> = Vec::new();
    while cursor < tokens.len() {
        let token = tokens[cursor];
        if token.kind == TokenKind::Identifier
            && !token.text.starts_with('@')
            && !type_keyword_boundary(token.text)
        {
            type_parts.push(token.text.to_string());
            cursor += 1;
            if tokens.get(cursor).is_some_and(|t| t.text == ".") {
                type_parts.push(".".to_string());
                cursor += 1;
                continue;
            }
            break;
        }
        break;
    }
    if type_parts.is_empty() {
        return Err(err("expected a data type"));
    }
    let mut size = String::new();
    if tokens.get(cursor).is_some_and(|token| token.text == "(") {
        let start = tokens[cursor].start;
        let mut depth = 0usize;
        while cursor < tokens.len() {
            let token = tokens[cursor];
            if token.text == "(" {
                depth += 1;
            } else if token.text == ")" {
                depth -= 1;
                if depth == 0 {
                    size = sql[start..token.end].to_string();
                    cursor += 1;
                    break;
                }
            }
            cursor += 1;
        }
        if size.is_empty() {
            return Err(err("unterminated type size"));
        }
    }

    let mut default_expr = None;
    let mut is_output = false;
    let mut is_readonly = false;

    while let Some(token) = tokens.get(cursor) {
        if token.text == "=" {
            let (expr, next) = read_default(sql, tokens, cursor + 1, paren_list);
            if expr.is_empty() {
                return Err(err("expected a default expression after ="));
            }
            default_expr = Some(expr);
            cursor = next;
            continue;
        }
        if token_eq(*token, "OUTPUT") || token_eq(*token, "OUT") {
            is_output = true;
            cursor += 1;
            continue;
        }
        if token_eq(*token, "READONLY") {
            is_readonly = true;
            cursor += 1;
            continue;
        }
        break;
    }

    let base_type = type_parts
        .last()
        .map(|part| {
            part.trim_matches(|c| c == '[' || c == ']')
                .to_ascii_uppercase()
        })
        .unwrap_or_default();
    let mut data_type = type_parts.concat().to_ascii_lowercase();
    data_type.push_str(&size.replace(' ', ""));

    Ok((
        ProcParam {
            name,
            data_type,
            base_type: base_type.clone(),
            family: type_family(&base_type),
            is_output,
            is_readonly,
            default_expr,
        },
        cursor,
    ))
}

/// Keywords that terminate a type-name scan when they appear where a type
/// token could sit (they begin the default/output modifiers or the body).
fn type_keyword_boundary(text: &str) -> bool {
    ["AS", "WITH", "OUTPUT", "OUT", "READONLY", "FOR"]
        .iter()
        .any(|keyword| text.eq_ignore_ascii_case(keyword))
}

/// Reads a default expression from after `=` until a top-level `,`, the `)`
/// closing the parameter list, or (bare form) the `AS` keyword.
fn read_default(
    sql: &str,
    tokens: &[Token<'_>],
    start: usize,
    paren_list: bool,
) -> (String, usize) {
    let mut cursor = start;
    let mut depth = 0usize;
    let mut end_byte = None;
    let mut start_byte = None;
    while cursor < tokens.len() {
        let token = tokens[cursor];
        if depth == 0 {
            let terminates = token.text == ","
                || (paren_list && token.text == ")")
                || (!paren_list && token_eq(token, "AS"));
            if terminates {
                break;
            }
            if token_eq(token, "OUTPUT") || token_eq(token, "OUT") || token_eq(token, "READONLY") {
                break;
            }
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

fn is_param_token(text: &str) -> bool {
    text.starts_with('@') && !text.starts_with("@@") && text.len() > 1
}

pub(crate) fn token_eq(token: Token<'_>, keyword: &str) -> bool {
    token.text.eq_ignore_ascii_case(keyword)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(sql: &str) -> ProcHeader {
        parse_proc_header(sql).expect("header should parse")
    }

    #[test]
    fn detects_create_variants() {
        assert_eq!(
            detect_proc_script("CREATE PROCEDURE dbo.Foo AS SELECT 1"),
            Some(ProcVerb::Create)
        );
        assert_eq!(
            detect_proc_script("-- lead\nCREATE OR ALTER PROC dbo.Foo AS SELECT 1"),
            Some(ProcVerb::CreateOrAlter)
        );
        assert_eq!(
            detect_proc_script("/* x */ ALTER PROCEDURE Foo AS SELECT 1"),
            Some(ProcVerb::Alter)
        );
        assert_eq!(detect_proc_script("SELECT 1"), None);
        assert_eq!(detect_proc_script("CREATE TABLE t (a int)"), None);
        assert_eq!(detect_proc_script("CREATE VIEW v AS SELECT 1"), None);
        assert_eq!(detect_proc_script(""), None);
    }

    #[test]
    fn parses_simple_header() {
        let header = header(
            "CREATE PROCEDURE [dbo].[GetOrders] @id int, @name nvarchar(50) = N'x' OUT AS SELECT 1",
        );
        assert_eq!(header.verb, ProcVerb::Create);
        assert_eq!(header.schema.as_deref(), Some("dbo"));
        assert_eq!(header.name, "GetOrders");
        assert_eq!(header.params.len(), 2);
        assert_eq!(header.params[0].name, "id");
        assert_eq!(header.params[0].data_type, "int");
        assert_eq!(header.params[0].family, TypeFamily::Integer);
        assert!(!header.params[0].is_output);
        assert_eq!(header.params[1].name, "name");
        assert_eq!(header.params[1].data_type, "nvarchar(50)");
        assert_eq!(header.params[1].default_expr.as_deref(), Some("N'x'"));
        assert!(header.params[1].is_output);
    }

    #[test]
    fn parses_bare_form_and_readonly() {
        let header =
            header("ALTER PROC Foo @ids dbo.IntList READONLY, @n decimal(10, 2) AS SELECT 1");
        assert_eq!(header.verb, ProcVerb::Alter);
        assert_eq!(header.schema, None);
        assert_eq!(header.params[0].data_type, "dbo.intlist");
        assert!(header.params[0].is_readonly);
        assert!(!header.params[0].editable());
        assert_eq!(header.params[1].data_type, "decimal(10,2)");
    }

    #[test]
    fn parses_defaults_with_commas_and_calls() {
        let header = header(
            "CREATE PROC P @a int = dbo.Compute(1, 2), @b datetime = '2024-01-01' AS SELECT 1",
        );
        assert_eq!(
            header.params[0].default_expr.as_deref(),
            Some("dbo.Compute(1, 2)")
        );
        assert_eq!(
            header.params[1].default_expr.as_deref(),
            Some("'2024-01-01'")
        );
    }

    #[test]
    fn parses_temp_procedure() {
        let header = header("CREATE PROC #tmp @x bit AS SELECT 1");
        assert_eq!(header.name, "#tmp");
        assert_eq!(header.params[0].family, TypeFamily::Bit);
    }

    #[test]
    fn rewrite_flips_verb_preserving_body() {
        let sql = "-- comment\nCREATE PROCEDURE dbo.Foo @a int AS\nSELECT @a";
        let parsed = header(sql);
        let rewritten = rewrite_deploy(sql, &parsed, ProcVerb::Alter);
        assert_eq!(
            rewritten,
            "-- comment\nALTER PROCEDURE dbo.Foo @a int AS\nSELECT @a"
        );
        let reparsed = header(&rewritten);
        let back = rewrite_deploy(&rewritten, &reparsed, ProcVerb::Create);
        assert_eq!(back, sql);
    }

    #[test]
    fn literal_validation() {
        let int_param = ProcParam {
            name: "n".into(),
            data_type: "int".into(),
            base_type: "INT".into(),
            family: TypeFamily::Integer,
            is_output: false,
            is_readonly: false,
            default_expr: None,
        };
        assert_eq!(sql_literal("42", &int_param).unwrap(), "42");
        assert!(sql_literal("abc", &int_param).is_err());
        assert!(sql_literal("999999999999", &int_param).is_err());

        let str_param = ProcParam {
            base_type: "NVARCHAR".into(),
            family: TypeFamily::String,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("o'clock", &str_param).unwrap(), "N'o''clock'");

        let bit = ProcParam {
            base_type: "BIT".into(),
            family: TypeFamily::Bit,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("true", &bit).unwrap(), "1");
        assert!(sql_literal("yes", &bit).is_err());

        let date = ProcParam {
            base_type: "DATE".into(),
            family: TypeFamily::DateTime,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("2024-02-29", &date).unwrap(), "'2024-02-29'");
        assert!(sql_literal("2023-02-29", &date).is_err());
        assert!(sql_literal("2024-01-01 10:00", &date).is_err());

        let guid = ProcParam {
            base_type: "UNIQUEIDENTIFIER".into(),
            family: TypeFamily::Guid,
            ..int_param.clone()
        };
        assert!(sql_literal("00000000-0000-0000-0000-000000000000", &guid).is_ok());
        assert!(sql_literal("not-a-guid", &guid).is_err());

        let bin = ProcParam {
            base_type: "VARBINARY".into(),
            family: TypeFamily::Binary,
            ..int_param.clone()
        };
        assert_eq!(sql_literal("0xDEADbeef", &bin).unwrap(), "0xDEADbeef");
        assert!(sql_literal("DEAD", &bin).is_err());
        assert!(sql_literal("0xABC", &bin).is_err());

        let dt2 = ProcParam {
            base_type: "DATETIME2".into(),
            family: TypeFamily::DateTime,
            ..int_param.clone()
        };
        assert_eq!(
            sql_literal("2026-07-28 14:30:15.1234567", &dt2).unwrap(),
            "'2026-07-28 14:30:15.1234567'"
        );
        assert!(sql_literal("2026-07-28 14:30:15.12345678", &dt2).is_err());
        assert!(sql_literal("2026-07-28 14:30:15.", &dt2).is_err());
        assert!(sql_literal("2026-07-28 14:30:15.abc", &dt2).is_err());
    }

    #[test]
    fn invoke_batch_captures_outputs_and_return_code() {
        let header = header(
            "CREATE PROCEDURE dbo.Foo @in int, @out nvarchar(10) OUTPUT, @skipped int = 5 AS SELECT 1",
        );
        let batch = build_invoke_batch(
            &header,
            &[
                ParamArg {
                    name: "in".into(),
                    value: Some("7".into()),
                },
                ParamArg {
                    name: "out".into(),
                    value: None,
                },
            ],
        )
        .unwrap();
        assert!(batch.contains("DECLARE [@in] int = 7;"));
        assert!(batch.contains("DECLARE [@out] nvarchar(10);"));
        assert!(
            batch.contains("EXEC @__sqlly_rc = [dbo].[Foo] [@in] = [@in], [@out] = [@out] OUTPUT;")
        );
        assert!(batch.contains("SELECT @__sqlly_rc AS [__sqlly_rc], [@out] AS [out];"));
        assert!(!batch.contains("skipped"));
    }

    #[test]
    fn invoke_batch_requires_values_without_defaults() {
        let header = header("CREATE PROC dbo.Foo @in int AS SELECT 1");
        let err = build_invoke_batch(&header, &[]).unwrap_err();
        assert_eq!(err.param, "in");
    }

    #[test]
    fn invoke_batch_rejects_tvp_values() {
        let header = header("CREATE PROC dbo.Foo @t dbo.IntList READONLY AS SELECT 1");
        let err = build_invoke_batch(
            &header,
            &[ParamArg {
                name: "t".into(),
                value: Some("1".into()),
            }],
        )
        .unwrap_err();
        assert_eq!(err.param, "t");
    }

    #[test]
    fn probe_sql_quotes_names() {
        let header = header("CREATE PROCEDURE [we'ird].[P]]x] AS SELECT 1");
        let probe = object_probe_sql(&header);
        assert!(probe.contains("N'we''ird.P]x'"));
    }

    #[test]
    fn parse_errors_are_clear() {
        assert!(parse_proc_header("SELECT 1").is_err());
        assert!(parse_proc_header("CREATE PROC AS SELECT 1").is_err());
        assert!(parse_proc_header("CREATE PROC P @x AS SELECT 1").is_err());
    }

    #[test]
    fn rollback_batch_wraps_deploy_in_sp_executesql() {
        let batch = build_rollback_batch(
            "CREATE PROC dbo.Foo AS SELECT 'x'",
            "EXEC @__sqlly_rc = [dbo].[Foo];",
        );
        assert!(batch.starts_with("SET XACT_ABORT ON;"));
        assert!(batch.contains("BEGIN TRANSACTION;"));
        assert!(batch.contains("EXEC sys.sp_executesql N'CREATE PROC dbo.Foo AS SELECT ''x''';"));
        assert!(batch.contains("EXEC @__sqlly_rc = [dbo].[Foo];"));
        assert!(batch.ends_with("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;"));
    }

    #[test]
    fn head_scan_caps_huge_leading_comments() {
        let mut sql = "-- ".to_string();
        sql.push_str(&"x".repeat(128 * 1024));
        sql.push_str("\nCREATE PROC dbo.Foo AS SELECT 1");
        // The verb sits beyond the scan cap: treated as not-a-proc-script
        // rather than paying a full-buffer lex on every keystroke.
        assert_eq!(detect_proc_script(&sql), None);
        let within = format!("-- {}\nCREATE PROC dbo.Foo AS SELECT 1", "x".repeat(1024));
        assert_eq!(detect_proc_script(&within), Some(ProcVerb::Create));
    }
}
