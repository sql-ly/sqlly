//! Lightweight DDL statement parser built on the lexical token stream.
//!
//! [`simple_tokenize_model`] scans the [`crate::tokenizer::tokenize`] output for `CREATE
//! TABLE/VIEW/PROCEDURE/FUNCTION/TYPE` statements and builds a
//! [`crate::fsmodel::SqlModelSnapshot`] — columns (type, default, foreign key,
//! check, identity, nullability), parameters (in/out), and result sets. It is
//! intentionally a single linear pass with no backtracking so it stays fast on
//! large source trees (per AGENTS.md); the token-scanning helpers below
//! ([`matching_paren`], [`split_top_level`], [`find_keyword`], ...) all walk
//! forward over an already-materialized `&[Token]` slice and never re-lex.

use std::collections::BTreeMap;

use crate::fsmodel::{
    FileSourceRef, FileSourcedSqlNode, FunctionSqlNode, NamedSqlNode, OperationSqlNode,
    ParseConfidence, ProcedureSqlNode, SchemaSqlNode, SourceSpan, SqlColumnNode, SqlDataType,
    SqlModelDiagnostic, SqlModelSnapshot, SqlNodeId, SqlNodeKind, SqlParameterNode,
    SqlResultColumnNode, SqlResultSetNode, SqlTypeLength, TableSqlNode, TypeSqlNode, ViewSqlNode,
};

use crate::tokenizer::{
    Token, TokenKind, TokenizationResult, is_builtin_type, parse_diagnostics, tokenize,
};

pub fn simple_tokenize_model(sql: &str, source: FileSourceRef) -> TokenizationResult {
    let tokens = significant_tokens(sql);
    let mut builder = SnapshotBuilder::default();
    let mut i = 0;

    while i < tokens.len() {
        if !token_eq(tokens[i], "CREATE") {
            i += 1;
            continue;
        }
        let object_index = skip_create_modifiers(&tokens, i + 1);
        if object_index >= tokens.len() {
            break;
        }
        let next = upper(tokens[object_index].text);
        let consumed = match next.as_str() {
            "TABLE" => parse_create_table(&tokens, object_index + 1, &source, &mut builder),
            "VIEW" => parse_create_view(&tokens, object_index + 1, &source, &mut builder),
            "PROCEDURE" | "PROC" => {
                parse_create_procedure(&tokens, object_index + 1, &source, &mut builder)
            }
            "TYPE" => parse_create_type(&tokens, object_index + 1, &source, &mut builder),
            "FUNCTION" => parse_create_function(&tokens, object_index + 1, &source, &mut builder),
            _ => None,
        };
        i = consumed.unwrap_or(object_index + 1);
    }

    for diagnostic in parse_diagnostics(sql) {
        builder.diagnostics.push(SqlModelDiagnostic {
            message: diagnostic,
            span: None,
        });
    }

    TokenizationResult {
        snapshot: builder.finish(),
    }
}

fn parse_create_table(
    tokens: &[Token<'_>],
    start: usize,
    source: &FileSourceRef,
    builder: &mut SnapshotBuilder,
) -> Option<usize> {
    let (parts, next) = parse_multipart_identifier(tokens, start)?;
    let (schema, name) = schema_and_name(&parts);
    let object_span = span_from(
        tokens[start],
        tokens.get(next).copied().unwrap_or(tokens[start]),
    );
    let source_node = file_node(
        SqlNodeKind::Table,
        schema.clone(),
        name.clone(),
        object_span,
        source.clone(),
    );
    let mut columns = Vec::new();
    if tokens.get(next).map(|token| token.text) == Some("(")
        && let Some(end) = matching_paren(tokens, next)
    {
        columns = parse_column_list(
            tokens,
            next + 1,
            end,
            schema.as_deref(),
            &source_node.node.id,
        );
        builder.add_table(
            schema,
            TableSqlNode {
                source_node,
                columns,
            },
        );
        return Some(end + 1);
    }
    builder.add_table(
        schema,
        TableSqlNode {
            source_node,
            columns,
        },
    );
    Some(next)
}

fn parse_create_view(
    tokens: &[Token<'_>],
    start: usize,
    source: &FileSourceRef,
    builder: &mut SnapshotBuilder,
) -> Option<usize> {
    let (parts, mut next) = parse_multipart_identifier(tokens, start)?;
    let (schema, name) = schema_and_name(&parts);
    let object_span = span_from(
        tokens[start],
        tokens.get(next).copied().unwrap_or(tokens[start]),
    );
    let source_node = file_node(
        SqlNodeKind::View,
        schema.clone(),
        name,
        object_span,
        source.clone(),
    );
    let mut columns = Vec::new();
    if tokens.get(next).map(|token| token.text) == Some("(")
        && let Some(end) = matching_paren(tokens, next)
    {
        columns = parse_view_columns(
            tokens,
            next + 1,
            end,
            schema.as_deref(),
            &source_node.node.id,
        );
        next = end + 1;
    }
    let select_span = tokens
        .iter()
        .skip(next)
        .find(|token| token_eq(**token, "SELECT"))
        .map(|token| SourceSpan {
            start: token.start,
            end: token.end,
        });
    builder.add_view(
        schema,
        ViewSqlNode {
            source_node,
            columns,
            select_span,
        },
    );
    Some(next)
}

fn parse_create_procedure(
    tokens: &[Token<'_>],
    start: usize,
    source: &FileSourceRef,
    builder: &mut SnapshotBuilder,
) -> Option<usize> {
    let (parts, next) = parse_multipart_identifier(tokens, start)?;
    let body_start = find_keyword(tokens, next, "AS").unwrap_or(tokens.len());
    let (schema, name) = schema_and_name(&parts);
    let source_node = file_node(
        SqlNodeKind::Procedure,
        schema.clone(),
        name.clone(),
        span_from(tokens[start], tokens[next - 1]),
        source.clone(),
    );
    let input_parameters = parse_parameters(
        tokens,
        next,
        body_start,
        schema.as_deref(),
        &source_node.node.id,
    );
    let result_sets =
        parse_result_sets(tokens, body_start, schema.as_deref(), &source_node.node.id);
    builder.add_procedure(
        schema,
        ProcedureSqlNode {
            operation: OperationSqlNode {
                source_node,
                input_parameters,
                result_sets,
            },
        },
    );
    Some(body_start.saturating_add(1))
}

fn parse_create_type(
    tokens: &[Token<'_>],
    start: usize,
    source: &FileSourceRef,
    builder: &mut SnapshotBuilder,
) -> Option<usize> {
    let (parts, mut next) = parse_multipart_identifier(tokens, start)?;
    let (schema, name) = schema_and_name(&parts);
    let source_node = file_node(
        SqlNodeKind::Type,
        schema.clone(),
        name.clone(),
        span_from(tokens[start], tokens[next - 1]),
        source.clone(),
    );
    let mut underlying_type = SqlDataType::Unknown;
    let mut columns = Vec::new();
    if token_eq(*tokens.get(next)?, "FROM") {
        let (data_type, after_type) = parse_data_type(tokens, next + 1);
        underlying_type = data_type;
        next = after_type;
    } else if token_eq(*tokens.get(next)?, "AS") && token_eq(*tokens.get(next + 1)?, "TABLE") {
        next += 2;
        if tokens.get(next).map(|token| token.text) == Some("(")
            && let Some(end) = matching_paren(tokens, next)
        {
            columns = parse_column_list(
                tokens,
                next + 1,
                end,
                schema.as_deref(),
                &source_node.node.id,
            );
            next = end + 1;
        }
    }
    builder.add_type(
        schema,
        TypeSqlNode {
            source_node,
            underlying_type,
            columns,
        },
    );
    Some(next)
}

fn parse_create_function(
    tokens: &[Token<'_>],
    start: usize,
    source: &FileSourceRef,
    builder: &mut SnapshotBuilder,
) -> Option<usize> {
    let (parts, next) = parse_multipart_identifier(tokens, start)?;
    let (schema, name) = schema_and_name(&parts);
    let source_node = file_node(
        SqlNodeKind::Function,
        schema.clone(),
        name.clone(),
        span_from(tokens[start], tokens[next - 1]),
        source.clone(),
    );
    let mut parameter_start = next;
    let mut parameter_end = next;
    if tokens.get(next).map(|token| token.text) == Some("(")
        && let Some(end) = matching_paren(tokens, next)
    {
        parameter_start = next + 1;
        parameter_end = end;
    }
    let input_parameters = parse_parameters(
        tokens,
        parameter_start,
        parameter_end,
        schema.as_deref(),
        &source_node.node.id,
    );
    let returns = find_keyword(tokens, parameter_end, "RETURNS")
        .map(|index| parse_data_type(tokens, index + 1).0)
        .unwrap_or(SqlDataType::Unknown);
    let result_sets = parse_result_sets(
        tokens,
        parameter_end,
        schema.as_deref(),
        &source_node.node.id,
    );
    builder.add_function(
        schema,
        FunctionSqlNode {
            operation: OperationSqlNode {
                source_node,
                input_parameters,
                result_sets,
            },
            output: returns,
        },
    );
    Some(parameter_end.saturating_add(1))
}

fn parse_column_list(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    schema: Option<&str>,
    parent: &SqlNodeId,
) -> Vec<SqlColumnNode> {
    split_top_level(tokens, start, end, ",")
        .into_iter()
        .filter_map(|(segment_start, segment_end)| {
            parse_column(tokens, segment_start, segment_end, schema, parent)
        })
        .collect()
}

fn parse_column(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    schema: Option<&str>,
    parent: &SqlNodeId,
) -> Option<SqlColumnNode> {
    let first = *tokens.get(start)?;
    if matches!(
        upper(first.text).as_str(),
        "CONSTRAINT" | "PRIMARY" | "FOREIGN" | "CHECK" | "UNIQUE"
    ) {
        return None;
    }
    let name = clean_identifier(first.text);
    let (data_type, after_type) = parse_data_type(tokens, start + 1);
    let mut node = NamedSqlNode::new(
        SqlNodeKind::Column,
        schema.map(str::to_string),
        Some(parent.clone()),
        name,
    )
    .with_data_type(data_type)
    .with_confidence(ParseConfidence::Partial)
    .with_span(span_from(first, tokens[end - 1]));
    if matches!(node.data_type, SqlDataType::Unknown) {
        node.confidence = ParseConfidence::Unknown;
    }
    let nullable = parse_nullability(tokens, after_type, end);
    let is_identity = contains_keyword(tokens, after_type, end, "IDENTITY");
    let default = parse_clause_text(tokens, after_type, end, "DEFAULT");
    let foreign_key = parse_clause_text(tokens, after_type, end, "REFERENCES");
    let check_constraints = parse_all_clause_text(tokens, after_type, end, "CHECK");
    Some(SqlColumnNode {
        node,
        nullable,
        is_identity,
        default,
        foreign_key,
        check_constraints,
    })
}

fn parse_view_columns(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    schema: Option<&str>,
    parent: &SqlNodeId,
) -> Vec<SqlColumnNode> {
    split_top_level(tokens, start, end, ",")
        .into_iter()
        .filter_map(|(segment_start, _)| {
            let token = *tokens.get(segment_start)?;
            Some(SqlColumnNode {
                node: NamedSqlNode::new(
                    SqlNodeKind::Column,
                    schema.map(str::to_string),
                    Some(parent.clone()),
                    clean_identifier(token.text),
                )
                .with_confidence(ParseConfidence::Exact)
                .with_span(SourceSpan {
                    start: token.start,
                    end: token.end,
                }),
                nullable: None,
                is_identity: false,
                default: None,
                foreign_key: None,
                check_constraints: Vec::new(),
            })
        })
        .collect()
}

fn parse_parameters(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    schema: Option<&str>,
    parent: &SqlNodeId,
) -> Vec<SqlParameterNode> {
    split_top_level(tokens, start, end, ",")
        .into_iter()
        .filter_map(|(segment_start, segment_end)| {
            let first = *tokens.get(segment_start)?;
            if !first.text.starts_with('@') {
                return None;
            }
            let (data_type, after_type) = parse_data_type(tokens, segment_start + 1);
            Some(SqlParameterNode {
                node: NamedSqlNode::new(
                    SqlNodeKind::Parameter,
                    schema.map(str::to_string),
                    Some(parent.clone()),
                    clean_identifier(first.text),
                )
                .with_data_type(data_type)
                .with_confidence(ParseConfidence::Partial)
                .with_span(span_from(first, tokens[segment_end - 1])),
                default: parse_assignment_default(tokens, after_type, segment_end),
                is_output: contains_keyword(tokens, after_type, segment_end, "OUTPUT")
                    || contains_keyword(tokens, after_type, segment_end, "OUT"),
                is_readonly: contains_keyword(tokens, after_type, segment_end, "READONLY"),
            })
        })
        .collect()
}

fn parse_result_sets(
    tokens: &[Token<'_>],
    start: usize,
    schema: Option<&str>,
    parent: &SqlNodeId,
) -> Vec<SqlResultSetNode> {
    let mut sets = Vec::new();
    let mut index = start;
    while let Some(select_index) = find_keyword(tokens, index, "SELECT") {
        let from_index = find_keyword(tokens, select_index + 1, "FROM")
            .unwrap_or_else(|| find_statement_end(tokens, select_index + 1));
        let result_id = SqlNodeId::new(
            SqlNodeKind::ResultSet,
            schema,
            Some(parent.0.as_str()),
            &format!("resultset{}", sets.len() + 1),
        );
        let columns = split_top_level(tokens, select_index + 1, from_index, ",")
            .into_iter()
            .enumerate()
            .filter_map(|(ordinal, (segment_start, segment_end))| {
                let name = infer_select_column_name(tokens, segment_start, segment_end)
                    .unwrap_or_else(|| format!("column{}", ordinal + 1));
                Some(SqlResultColumnNode {
                    node: NamedSqlNode::new(
                        SqlNodeKind::ResultSetColumn,
                        schema.map(str::to_string),
                        Some(result_id.clone()),
                        name,
                    )
                    .with_data_type(SqlDataType::Unknown)
                    .with_confidence(ParseConfidence::Inferred)
                    .with_span(span_from(
                        *tokens.get(segment_start)?,
                        tokens[segment_end - 1],
                    )),
                })
            })
            .collect();
        sets.push(SqlResultSetNode {
            node: NamedSqlNode::new(
                SqlNodeKind::ResultSet,
                schema.map(str::to_string),
                Some(parent.clone()),
                format!("resultset{}", sets.len() + 1),
            )
            .with_confidence(ParseConfidence::Inferred)
            .with_span(SourceSpan {
                start: tokens[select_index].start,
                end: tokens[from_index.saturating_sub(1)].end,
            }),
            columns,
        });
        index = find_statement_end(tokens, select_index + 1).saturating_add(1);
    }
    sets
}

fn parse_data_type(tokens: &[Token<'_>], start: usize) -> (SqlDataType, usize) {
    let Some(first) = tokens.get(start).copied() else {
        return (SqlDataType::Unknown, start);
    };
    if first.text == ")" || first.text == "," {
        return (SqlDataType::Unknown, start);
    }
    let mut parts = vec![clean_identifier(first.text)];
    let mut index = start + 1;
    if tokens.get(index).map(|token| token.text) == Some(".")
        && let Some(second) = tokens.get(index + 1)
    {
        parts.push(clean_identifier(second.text));
        index += 2;
    }
    let mut length = None;
    let mut precision = None;
    let mut scale = None;
    if tokens.get(index).map(|token| token.text) == Some("(")
        && let Some(end) = matching_paren(tokens, index)
    {
        let args: Vec<_> = tokens[index + 1..end]
            .iter()
            .filter(|token| token.text != ",")
            .map(|token| token.text)
            .collect();
        if args.len() == 1 && args[0].eq_ignore_ascii_case("MAX") {
            length = Some(SqlTypeLength::Max);
        } else if args.len() == 1 {
            length = args[0].parse::<u32>().ok().map(SqlTypeLength::Value);
        } else if args.len() >= 2 {
            precision = args[0].parse::<u8>().ok();
            scale = args[1].parse::<u8>().ok();
        }
        index = end + 1;
    }
    let type_name = parts.last().cloned().unwrap_or_default();
    let data_type = if is_builtin_type(&type_name) {
        SqlDataType::BuiltIn {
            name: type_name.to_ascii_uppercase(),
            length,
            precision,
            scale,
        }
    } else if parts.len() == 2 {
        SqlDataType::Custom {
            schema: Some(parts[0].clone()),
            name: parts[1].clone(),
        }
    } else {
        SqlDataType::Custom {
            schema: None,
            name: type_name,
        }
    };
    (data_type, index)
}

fn file_node(
    kind: SqlNodeKind,
    schema: Option<String>,
    name: String,
    span: SourceSpan,
    source: FileSourceRef,
) -> FileSourcedSqlNode {
    FileSourcedSqlNode {
        node: NamedSqlNode::new(kind, schema, None, name)
            .with_confidence(ParseConfidence::Exact)
            .with_span(span),
        source,
    }
}

#[derive(Default)]
struct SnapshotBuilder {
    schemas: BTreeMap<String, SchemaSqlNode>,
    diagnostics: Vec<SqlModelDiagnostic>,
}

impl SnapshotBuilder {
    fn schema_mut(&mut self, schema: Option<String>) -> &mut SchemaSqlNode {
        let name = schema.unwrap_or_else(|| "dbo".to_string());
        self.schemas
            .entry(name.clone())
            .or_insert_with(|| SchemaSqlNode {
                node: NamedSqlNode::new(SqlNodeKind::Schema, Some(name.clone()), None, name)
                    .with_confidence(ParseConfidence::Exact),
                tables: Vec::new(),
                views: Vec::new(),
                functions: Vec::new(),
                types: Vec::new(),
                procedures: Vec::new(),
            })
    }

    fn add_table(&mut self, schema: Option<String>, table: TableSqlNode) {
        self.schema_mut(schema).tables.push(table);
    }

    fn add_view(&mut self, schema: Option<String>, view: ViewSqlNode) {
        self.schema_mut(schema).views.push(view);
    }

    fn add_procedure(&mut self, schema: Option<String>, procedure: ProcedureSqlNode) {
        self.schema_mut(schema).procedures.push(procedure);
    }

    fn add_function(&mut self, schema: Option<String>, function: FunctionSqlNode) {
        self.schema_mut(schema).functions.push(function);
    }

    fn add_type(&mut self, schema: Option<String>, custom_type: TypeSqlNode) {
        self.schema_mut(schema).types.push(custom_type);
    }

    fn finish(self) -> SqlModelSnapshot {
        SqlModelSnapshot {
            schemas: self.schemas.into_values().collect(),
            diagnostics: self.diagnostics,
        }
    }
}

fn significant_tokens(sql: &str) -> Vec<Token<'_>> {
    tokenize(sql)
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect()
}

fn skip_create_modifiers(tokens: &[Token<'_>], start: usize) -> usize {
    if token_eq(*tokens.get(start).unwrap_or(&tokens[0]), "OR")
        && token_eq(*tokens.get(start + 1).unwrap_or(&tokens[0]), "ALTER")
    {
        start + 2
    } else {
        start
    }
}

fn parse_multipart_identifier(tokens: &[Token<'_>], start: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = Vec::new();
    let mut index = start;
    loop {
        let token = *tokens.get(index)?;
        if !matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword) {
            break;
        }
        parts.push(clean_identifier(token.text));
        index += 1;
        if tokens.get(index).map(|token| token.text) != Some(".") {
            break;
        }
        index += 1;
    }
    (!parts.is_empty()).then_some((parts, index))
}

fn schema_and_name(parts: &[String]) -> (Option<String>, String) {
    match parts {
        [] => (None, String::new()),
        [name] => (Some("dbo".to_string()), name.clone()),
        _ => (
            Some(parts[parts.len() - 2].clone()),
            parts[parts.len() - 1].clone(),
        ),
    }
}

fn matching_paren(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.text {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    separator: &str,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut segment_start = start;
    for (offset, token) in tokens[start..end].iter().enumerate() {
        let index = start + offset;
        match token.text {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            text if text == separator && depth == 0 => {
                if segment_start < index {
                    ranges.push((segment_start, index));
                }
                segment_start = index + 1;
            }
            _ => {}
        }
    }
    if segment_start < end {
        ranges.push((segment_start, end));
    }
    ranges
}

fn find_keyword(tokens: &[Token<'_>], start: usize, keyword: &str) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, token)| token_eq(*token, keyword).then_some(index))
}

fn find_statement_end(tokens: &[Token<'_>], start: usize) -> usize {
    tokens
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, token)| (token.text == ";" || token_eq(*token, "GO")).then_some(index))
        .unwrap_or(tokens.len())
}

fn contains_keyword(tokens: &[Token<'_>], start: usize, end: usize, keyword: &str) -> bool {
    tokens[start..end]
        .iter()
        .any(|token| token_eq(*token, keyword))
}

fn parse_nullability(tokens: &[Token<'_>], start: usize, end: usize) -> Option<bool> {
    for index in start..end {
        if token_eq(tokens[index], "NOT")
            && tokens
                .get(index + 1)
                .is_some_and(|token| token_eq(*token, "NULL"))
        {
            return Some(false);
        }
        if token_eq(tokens[index], "NULL") {
            return Some(true);
        }
    }
    None
}

fn parse_clause_text(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    keyword: &str,
) -> Option<String> {
    parse_all_clause_text(tokens, start, end, keyword)
        .into_iter()
        .next()
}

fn parse_all_clause_text(
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    keyword: &str,
) -> Vec<String> {
    let mut clauses = Vec::new();
    for index in start..end {
        if token_eq(tokens[index], keyword) {
            let clause_end = (index + 1..end)
                .find(|candidate| is_constraint_boundary(tokens[*candidate]))
                .unwrap_or(end);
            clauses.push(join_tokens(tokens, index + 1, clause_end));
        }
    }
    clauses
}

fn parse_assignment_default(tokens: &[Token<'_>], start: usize, end: usize) -> Option<String> {
    (start..end)
        .find(|index| tokens[*index].text == "=")
        .map(|index| join_tokens(tokens, index + 1, end))
}

fn is_constraint_boundary(token: Token<'_>) -> bool {
    matches!(
        upper(token.text).as_str(),
        "CONSTRAINT" | "NOT" | "NULL" | "PRIMARY" | "FOREIGN" | "REFERENCES" | "CHECK" | "DEFAULT"
    )
}

fn infer_select_column_name(tokens: &[Token<'_>], start: usize, end: usize) -> Option<String> {
    for index in start..end.saturating_sub(1) {
        if token_eq(tokens[index], "AS") {
            return Some(clean_identifier(tokens[index + 1].text));
        }
    }
    tokens
        .get(end.checked_sub(1)?)
        .map(|token| clean_identifier(token.text))
        .filter(|name| name != "*")
}

fn join_tokens(tokens: &[Token<'_>], start: usize, end: usize) -> String {
    tokens[start..end]
        .iter()
        .map(|token| token.text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn span_from(start: Token<'_>, end: Token<'_>) -> SourceSpan {
    SourceSpan {
        start: start.start,
        end: end.end,
    }
}

fn clean_identifier(text: &str) -> String {
    if text.starts_with('[') && text.ends_with(']') {
        text[1..text.len() - 1].replace("]]", "]")
    } else {
        text.trim_start_matches('@').to_string()
    }
}

fn upper(text: &str) -> String {
    text.to_ascii_uppercase()
}

fn token_eq(token: Token<'_>, keyword: &str) -> bool {
    token.text.eq_ignore_ascii_case(keyword)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    fn source() -> FileSourceRef {
        FileSourceRef {
            source_file_name: "schema.sql".to_string(),
            source_file_path: "/tmp/schema.sql".to_string(),
            file_hash: "abc123".to_string(),
        }
    }

    #[test]
    fn tokenizes_utf8_bom_without_splitting_codepoint() {
        let sql = "\u{feff}CREATE TABLE dbo.Cafe (Name nvarchar(10));";
        let tokens = tokenize(sql);
        assert_eq!(tokens.first().unwrap().text, "\u{feff}");

        let result = simple_tokenize_model(sql, source());
        let dbo = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "dbo")
            .expect("dbo schema discovered");
        assert_eq!(dbo.tables[0].source_node.node.name, "Cafe");
    }

    #[test]
    fn golden_extracts_named_sql_nodes() {
        let sql = r#"
CREATE TABLE dbo.Customers (
    CustomerId int NOT NULL IDENTITY(1,1),
    RegionCode varchar(8) DEFAULT 'NA' REFERENCES dbo.Region(Code),
    Status int CHECK (Status IN (1,2))
);
GO
CREATE VIEW sales.CustomerList (CustomerId, RegionCode) AS SELECT CustomerId, RegionCode FROM dbo.Customers;
GO
CREATE PROCEDURE sales.FindCustomer @region varchar(8) = 'NA', @total int OUTPUT AS SELECT CustomerId, RegionCode FROM dbo.Customers;
GO
CREATE TYPE dbo.Code FROM varchar(12) NOT NULL;
GO
CREATE FUNCTION dbo.CustomerKey(@id int) RETURNS int AS BEGIN RETURN @id END;
"#;
        let result = simple_tokenize_model(sql, source());
        let schema_names: Vec<_> = result
            .snapshot
            .schemas
            .iter()
            .map(|schema| schema.node.name.as_str())
            .collect();
        assert_eq!(schema_names, vec!["dbo", "sales"]);

        let dbo = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "dbo")
            .unwrap();
        assert_eq!(dbo.tables[0].source_node.node.name, "Customers");
        assert_eq!(dbo.tables[0].columns.len(), 3);
        assert_eq!(dbo.tables[0].columns[1].default.as_deref(), Some("'NA'"));
        assert_eq!(
            dbo.tables[0].columns[1].foreign_key.as_deref(),
            Some("dbo . Region ( Code )")
        );
        assert_eq!(
            dbo.tables[0].columns[2].check_constraints,
            vec!["( Status IN ( 1 , 2 ) )".to_string()]
        );
        assert_eq!(dbo.types[0].source_node.node.name, "Code");
        assert_eq!(
            dbo.functions[0].operation.input_parameters[0].node.name,
            "id"
        );

        let sales = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "sales")
            .unwrap();
        assert_eq!(sales.views[0].columns.len(), 2);
        assert_eq!(sales.procedures[0].operation.input_parameters.len(), 2);
        assert_eq!(
            sales.procedures[0].operation.result_sets[0].columns[0]
                .node
                .name,
            "CustomerId"
        );
    }

    /// PostgreSQL range types are types, not functions, and a column with a
    /// `STORAGE` clause keeps its name and type (DataGrip DBE-19512,
    /// DBE-26419).
    #[test]
    fn postgres_range_types_and_column_storage() {
        let sql = "CREATE TYPE public.test_range_type AS RANGE (subtype = time, MULTIRANGE_TYPE_NAME = timemultirange);\n\
                   CREATE TYPE public.float8_range AS RANGE (subtype = float8, subtype_diff = float8mi);\n\
                   CREATE TABLE public.foo (bar TEXT STORAGE EXTENDED, baz int);";
        let result = simple_tokenize_model(sql, source());
        let public = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "public")
            .expect("public schema discovered");
        let types: Vec<_> = public
            .types
            .iter()
            .map(|t| t.source_node.node.name.as_str())
            .collect();
        assert_eq!(types, vec!["test_range_type", "float8_range"]);
        assert!(public.functions.is_empty());
        let columns: Vec<_> = public.tables[0]
            .columns
            .iter()
            .map(|c| c.node.name.as_str())
            .collect();
        assert_eq!(columns, vec!["bar", "baz"]);
    }

    #[test]
    fn golden_ignores_fake_sql_in_comments_and_strings() {
        let sql = "-- CREATE TABLE dbo.Fake (id int)\nSELECT 'CREATE VIEW dbo.Fake AS SELECT 1';\nCREATE TABLE dbo.Real (id int);";
        let result = simple_tokenize_model(sql, source());
        let dbo = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "dbo")
            .unwrap();
        assert_eq!(dbo.tables.len(), 1);
        assert_eq!(dbo.tables[0].source_node.node.name, "Real");
    }

    /// Drives the messy real-world fixture (`fixtures/tokenizer/messy.sql`):
    /// mixed casing, irregular whitespace, inline + block comments, string
    /// literals that mention CREATE/PROCEDURE, GO batch separators, quoted +
    /// bracketed identifiers, and a malformed trailing fragment. The linear
    /// scanner must extract the real objects without being fooled by the noise,
    /// and must not panic on the broken tail.
    #[test]
    fn handles_messy_real_world_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tokenizer/messy.sql"
        );
        let sql = std::fs::read_to_string(path).expect("messy fixture exists");
        let result = simple_tokenize_model(&sql, source());
        let dbo = result
            .snapshot
            .schemas
            .iter()
            .find(|schema| schema.node.name == "dbo")
            .expect("dbo schema discovered");

        // The well-formed table, the view, the proc, and the function are all
        // extracted; the string literals mentioning CREATE TABLE/PROCEDURE do
        // not create phantom objects.
        let table_names: Vec<&str> = dbo
            .tables
            .iter()
            .map(|t| t.source_node.node.name.as_str())
            .collect();
        assert!(
            table_names.contains(&"Weird Name"),
            "tables: {table_names:?}"
        );
        assert!(
            dbo.views
                .iter()
                .any(|v| v.source_node.node.name == "v_messy")
        );
        assert!(
            dbo.procedures
                .iter()
                .any(|p| p.operation.source_node.node.name == "usp_Messy")
        );
        assert!(
            dbo.functions
                .iter()
                .any(|f| f.operation.source_node.node.name == "fn_Messy")
        );

        // No phantom "Fake"/"should not confuse us" objects from string/comment
        // content leaked into the model.
        assert!(!table_names.iter().any(|n| n.contains("confuse")));
    }

    /// Performance budget guard (AGENTS.md: the tokenizer must stay performant).
    /// Builds a large synthetic schema and asserts the single-pass tokenizer
    /// stays well under budget. This is a coarse regression tripwire, not a
    /// micro-benchmark — it fails loudly if the scanner regresses to super-linear
    /// behavior. Budget is generous to avoid CI flakiness on shared runners.
    #[test]
    fn tokenizer_stays_within_latency_budget() {
        let mut sql = String::with_capacity(512 * 1024);
        for i in 0..2_000 {
            sql.push_str(&format!(
                "CREATE TABLE dbo.T{i} (\n  Id int IDENTITY(1,1) NOT NULL,\n  \
                 Name nvarchar(200) NULL DEFAULT N'x',\n  ParentId int NULL,\n  \
                 CONSTRAINT PK_T{i} PRIMARY KEY (Id),\n  \
                 CONSTRAINT CK_T{i} CHECK (LEN(Name) > 0)\n);\nGO\n"
            ));
        }
        let bytes = sql.len();
        let start = Instant::now();
        let result = simple_tokenize_model(&sql, source());
        let elapsed = start.elapsed();

        let dbo = result
            .snapshot
            .schemas
            .iter()
            .find(|s| s.node.name == "dbo")
            .expect("dbo discovered");
        assert_eq!(dbo.tables.len(), 2_000, "all tables extracted");
        assert!(
            elapsed.as_millis() < 1_500,
            "tokenizing {bytes} bytes took {elapsed:?}, over the 1500ms budget"
        );
    }

    #[test]
    fn matching_paren_tracks_nesting_and_rejects_unclosed() {
        let tokens = significant_tokens("( a ( b ) c )");
        assert_eq!(matching_paren(&tokens, 0), Some(6));
        assert_eq!(matching_paren(&tokens, 2), Some(4));

        let unclosed = significant_tokens("( a ( b )");
        assert_eq!(matching_paren(&unclosed, 0), None);
        assert_eq!(matching_paren(&unclosed, 2), Some(4));
    }

    #[test]
    fn split_top_level_ignores_nested_separators_and_empty_segments() {
        // a , b ( c , d ) , e — the comma inside parens must not split.
        let tokens = significant_tokens("a , b ( c , d ) , e");
        assert_eq!(
            split_top_level(&tokens, 0, tokens.len(), ","),
            vec![(0, 1), (2, 8), (9, 10)]
        );

        // Empty segments (consecutive separators) are dropped, not emitted
        // as zero-length ranges.
        let sparse = significant_tokens("a , , b");
        assert_eq!(
            split_top_level(&sparse, 0, sparse.len(), ","),
            vec![(0, 1), (3, 4)]
        );

        // An empty window yields no ranges.
        assert!(split_top_level(&tokens, 3, 3, ",").is_empty());
    }

    #[test]
    fn parse_data_type_covers_builtin_custom_and_sentinel_cases() {
        let max = significant_tokens("nvarchar(MAX) NOT NULL");
        assert_eq!(
            parse_data_type(&max, 0),
            (
                SqlDataType::BuiltIn {
                    name: "NVARCHAR".to_string(),
                    length: Some(SqlTypeLength::Max),
                    precision: None,
                    scale: None,
                },
                4
            )
        );

        let decimal = significant_tokens("decimal(10, 2)");
        assert_eq!(
            parse_data_type(&decimal, 0),
            (
                SqlDataType::BuiltIn {
                    name: "DECIMAL".to_string(),
                    length: None,
                    precision: Some(10),
                    scale: Some(2),
                },
                6
            )
        );

        // Two-part names resolve to a schema-qualified custom type.
        let custom = significant_tokens("dbo.MyType READONLY");
        assert_eq!(
            parse_data_type(&custom, 0),
            (
                SqlDataType::Custom {
                    schema: Some("dbo".to_string()),
                    name: "MyType".to_string(),
                },
                3
            )
        );

        // ')' and ',' are hard stops: no type is consumed and the index does
        // not advance (parse_column relies on this for bare column names).
        let stop = significant_tokens(", next");
        assert_eq!(parse_data_type(&stop, 0), (SqlDataType::Unknown, 0));
        assert_eq!(
            parse_data_type(&stop, stop.len()),
            (SqlDataType::Unknown, stop.len())
        );
    }
}
