use serde::{Deserialize, Serialize};
use sqlly_lang::tokenizer::{Token, TokenKind, tokenize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryParameterSet {
    pub parameters: Vec<QueryParameter>,
    pub diagnostics: Vec<QueryParameterDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryParameter {
    pub name: String,
    pub occurrences: Vec<ParameterOccurrence>,
    pub inferred_type: QueryParameterType,
    pub default_hint: Option<ParameterDefaultHint>,
    pub lookup_hint: Option<ParameterLookupHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterOccurrence {
    pub span: ParameterSourceSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterSourceSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QueryParameterType {
    Boolean,
    Integer,
    Decimal,
    Date,
    DateTime,
    Text,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterDefaultHint {
    pub expression: String,
    pub span: ParameterSourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterLookupHint {
    pub schema: Option<String>,
    pub table: String,
    pub value_column: String,
    pub display_column: String,
    pub span: ParameterSourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryParameterDiagnostic {
    pub message: String,
    pub span: Option<ParameterSourceSpan>,
}

pub fn detect_query_parameters(sql: &str) -> QueryParameterSet {
    let tokens = significant_tokens(sql);
    let declared = declared_assigned_parameters(&tokens);
    let declarations = declared_parameter_types(&tokens);
    let hints = comment_hints(sql);
    let mut by_name = std::collections::BTreeMap::<String, QueryParameter>::new();

    for token in tokens {
        if token.kind != TokenKind::Identifier || !is_parameter_token(token.text) {
            continue;
        }
        let name = normalize_parameter(token.text);
        if declared.contains(&name) {
            continue;
        }
        let entry = by_name
            .entry(name.clone())
            .or_insert_with(|| QueryParameter {
                inferred_type: declarations
                    .get(&name)
                    .cloned()
                    .unwrap_or(QueryParameterType::Unknown),
                default_hint: hints.defaults.get(&name).cloned(),
                lookup_hint: hints.lookups.get(&name).cloned(),
                name,
                occurrences: Vec::new(),
            });
        entry.occurrences.push(ParameterOccurrence {
            span: ParameterSourceSpan {
                start: token.start,
                end: token.end,
            },
        });
    }

    QueryParameterSet {
        parameters: by_name.into_values().collect(),
        diagnostics: hints.diagnostics,
    }
}

fn significant_tokens(sql: &str) -> Vec<Token<'_>> {
    tokenize(sql)
        .into_iter()
        .filter(|token| {
            !matches!(
                token.kind,
                TokenKind::Whitespace | TokenKind::Comment | TokenKind::StringLiteral
            )
        })
        .collect()
}

fn declared_assigned_parameters(tokens: &[Token<'_>]) -> std::collections::BTreeSet<String> {
    let mut locals = std::collections::BTreeSet::new();
    let mut index = 0;
    while index < tokens.len() {
        if !token_eq(tokens[index], "DECLARE") {
            index += 1;
            continue;
        }
        index += 1;
        while index < tokens.len() && tokens[index].text != ";" {
            if is_parameter_token(tokens[index].text) {
                let name = normalize_parameter(tokens[index].text);
                let end = find_segment_end(tokens, index + 1);
                if tokens[index + 1..end].iter().any(|token| token.text == "=") {
                    locals.insert(name);
                }
                index = end;
            }
            index += 1;
        }
    }
    locals
}

fn declared_parameter_types(
    tokens: &[Token<'_>],
) -> std::collections::BTreeMap<String, QueryParameterType> {
    let mut types = std::collections::BTreeMap::new();
    let mut index = 0;
    while index < tokens.len() {
        if !token_eq(tokens[index], "DECLARE") {
            index += 1;
            continue;
        }
        index += 1;
        while index < tokens.len() && tokens[index].text != ";" {
            if is_parameter_token(tokens[index].text) {
                let name = normalize_parameter(tokens[index].text);
                if let Some(data_type) = tokens
                    .get(index + 1)
                    .map(|token| parameter_type_from_sql(token.text))
                {
                    types.insert(name, data_type);
                }
            }
            index += 1;
        }
    }
    types
}

#[derive(Default)]
struct ParsedHints {
    defaults: std::collections::BTreeMap<String, ParameterDefaultHint>,
    lookups: std::collections::BTreeMap<String, ParameterLookupHint>,
    diagnostics: Vec<QueryParameterDiagnostic>,
}

fn comment_hints(sql: &str) -> ParsedHints {
    let mut hints = ParsedHints::default();
    // Walk real `--` comment tokens instead of scanning raw lines: `--` inside
    // a string literal is data (it used to hijack the line and swallow the
    // actual hint), a `--` inside a block comment is not a hint source, and
    // token offsets stay correct on CRLF files where a manual line counter
    // drifts.
    for token in tokenize(sql) {
        if token.kind != TokenKind::Comment || !token.text.starts_with("--") {
            continue;
        }
        let line_start = sql[..token.start].rfind('\n').map_or(0, |index| index + 1);
        let before = &sql[line_start..token.start];
        let comment = token.text[2..].trim();
        let span = ParameterSourceSpan {
            start: token.start,
            end: token.end,
        };
        if let Some((name, expression)) = parse_default_hint(comment) {
            hints
                .defaults
                .insert(name, ParameterDefaultHint { expression, span });
        }
        if let Some(parameter_name) = last_parameter_on_line(before) {
            match parse_lookup_hint(comment, span) {
                Ok(Some(lookup)) => {
                    hints.lookups.insert(parameter_name, lookup);
                }
                Ok(None) => {}
                Err(message) => hints.diagnostics.push(QueryParameterDiagnostic {
                    message,
                    span: Some(span),
                }),
            }
        }
    }
    hints
}

fn parse_default_hint(comment: &str) -> Option<(String, String)> {
    let (name, expression) = comment.split_once('=')?;
    let name = name.trim().trim_start_matches('@');
    if name.is_empty() || name.contains(char::is_whitespace) || name.starts_with('{') {
        return None;
    }
    Some((name.to_ascii_lowercase(), expression.trim().to_string()))
}

fn parse_lookup_hint(
    comment: &str,
    span: ParameterSourceSpan,
) -> Result<Option<ParameterLookupHint>, String> {
    let Some(open) = comment.find('{') else {
        return Ok(None);
    };
    let Some(close) = comment[open + 1..].find('}').map(|idx| open + 1 + idx) else {
        return Err("lookup hint is missing closing }".to_string());
    };
    let target = comment[open + 1..close].trim();
    let rest = comment[close + 1..].trim();
    let Some(col_open) = rest.find('{') else {
        return Err("lookup hint is missing value column".to_string());
    };
    let Some(col_close) = rest[col_open + 1..].find('}').map(|idx| col_open + 1 + idx) else {
        return Err("lookup hint value column is missing closing }".to_string());
    };
    let value_column = rest[col_open + 1..col_close].trim().to_string();
    if value_column.is_empty() {
        return Err("lookup hint value column is empty".to_string());
    }
    let display_column = if let Some(pipe) = rest[col_close + 1..]
        .find('|')
        .map(|idx| col_close + 1 + idx)
    {
        let after_pipe = rest[pipe + 1..].trim();
        if after_pipe.starts_with('{') && after_pipe.ends_with('}') && after_pipe.len() > 2 {
            after_pipe[1..after_pipe.len() - 1].trim().to_string()
        } else {
            value_column.clone()
        }
    } else {
        value_column.clone()
    };
    let parts: Vec<_> = target
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let (schema, table) = match parts.as_slice() {
        [table] => (None, (*table).to_string()),
        [schema, table] => (Some((*schema).to_string()), (*table).to_string()),
        _ => return Err("lookup hint target must be {table} or {schema}.{table}".to_string()),
    };
    Ok(Some(ParameterLookupHint {
        schema,
        table,
        value_column,
        display_column,
        span,
    }))
}

fn last_parameter_on_line(line: &str) -> Option<String> {
    let mut last = None;
    for token in tokenize(line) {
        if token.kind == TokenKind::Identifier && is_parameter_token(token.text) {
            last = Some(normalize_parameter(token.text));
        }
    }
    last
}

fn parameter_type_from_sql(sql_type: &str) -> QueryParameterType {
    match sql_type.to_ascii_uppercase().as_str() {
        "BIT" => QueryParameterType::Boolean,
        "BIGINT" | "INT" | "SMALLINT" | "TINYINT" => QueryParameterType::Integer,
        "DECIMAL" | "FLOAT" | "MONEY" | "NUMERIC" | "REAL" | "SMALLMONEY" => {
            QueryParameterType::Decimal
        }
        "DATE" => QueryParameterType::Date,
        "DATETIME" | "DATETIME2" | "DATETIMEOFFSET" | "SMALLDATETIME" | "TIME" => {
            QueryParameterType::DateTime
        }
        "CHAR" | "NCHAR" | "NTEXT" | "NVARCHAR" | "TEXT" | "VARCHAR" => QueryParameterType::Text,
        _ => QueryParameterType::Unknown,
    }
}

fn find_segment_end(tokens: &[Token<'_>], start: usize) -> usize {
    tokens[start..]
        .iter()
        .position(|token| token.text == "," || token.text == ";")
        .map_or(tokens.len(), |offset| start + offset)
}

fn is_parameter_token(text: &str) -> bool {
    text.starts_with('@') && !text.starts_with("@@") && text.len() > 1
}

fn normalize_parameter(text: &str) -> String {
    text.trim_start_matches('@').to_ascii_lowercase()
}

fn token_eq(token: Token<'_>, keyword: &str) -> bool {
    token.text.eq_ignore_ascii_case(keyword)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_external_parameters_and_spans() {
        let set = detect_query_parameters(
            "SELECT * FROM dbo.Orders WHERE CustomerId = @customerId AND Region = @region",
        );
        assert_eq!(
            set.parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["customerid", "region"]
        );
        assert_eq!(set.parameters[0].occurrences.len(), 1);
        assert!(set.diagnostics.is_empty());
    }

    #[test]
    fn excludes_declared_assigned_local_variables() {
        let set = detect_query_parameters("DECLARE @local int = 1; SELECT @local, @external");
        assert_eq!(
            set.parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["external"]
        );
    }

    #[test]
    fn parses_default_and_lookup_hints() {
        let set = detect_query_parameters(
            "SELECT * FROM dbo.Orders WHERE CreatedAt >= @yesterday -- yesterday = dateadd(day,-1,getdate())\n  AND CustomerId = @customer -- {dbo.Customers} {CustomerId}|{Name}",
        );
        let yesterday = set
            .parameters
            .iter()
            .find(|p| p.name == "yesterday")
            .unwrap();
        assert_eq!(
            yesterday.default_hint.as_ref().unwrap().expression,
            "dateadd(day,-1,getdate())"
        );
        let customer = set
            .parameters
            .iter()
            .find(|p| p.name == "customer")
            .unwrap();
        let lookup = customer.lookup_hint.as_ref().unwrap();
        assert_eq!(lookup.schema.as_deref(), Some("dbo"));
        assert_eq!(lookup.table, "Customers");
        assert_eq!(lookup.value_column, "CustomerId");
        assert_eq!(lookup.display_column, "Name");
    }

    #[test]
    fn string_literals_cannot_hijack_hint_comments() {
        // The `--` inside the string is data; the real comment after it must
        // still parse as a lookup hint for @id.
        let set = detect_query_parameters(
            "SELECT * FROM t WHERE note = '--not a comment' AND id = @id -- {dbo.T} {Id}|{Name}",
        );
        let id = set.parameters.iter().find(|p| p.name == "id").unwrap();
        let lookup = id.lookup_hint.as_ref().unwrap();
        assert_eq!(lookup.schema.as_deref(), Some("dbo"));
        assert_eq!(lookup.table, "T");
        assert_eq!(lookup.display_column, "Name");

        // A `--` inside a block comment is not a line-comment hint source.
        let set = detect_query_parameters("/* -- fake = 1 */ SELECT @fake");
        assert!(set.parameters[0].default_hint.is_none());
    }

    #[test]
    fn ignores_comments_strings_and_system_variables() {
        let set = detect_query_parameters("-- @comment\nSELECT '@string', @@spid, @real");
        assert_eq!(set.parameters.len(), 1);
        assert_eq!(set.parameters[0].name, "real");
    }
}
