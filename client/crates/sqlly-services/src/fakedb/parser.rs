//! Minimal SQL parsing for the demo subset.
//!
//! Recognized statement shapes (case-insensitive keywords):
//!
//! ```sql
//! SELECT * FROM name
//! SELECT TOP n col, col FROM name WHERE col op literal ORDER BY col DESC
//! SELECT COUNT(*) FROM name
//! SELECT ... LIMIT n
//! EXEC name 'arg', 2
//! ```
//!
//! This is intentionally not a general SQL parser — it is exactly what the
//! demo needs, with helpful errors for everything else.

use crate::fakedb::{FakeDbError, Value};

/// Strip `-- line` and `/* block */` comments, respecting string literals.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\'' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '\'' => {
                in_string = true;
                out.push(ch);
            }
            '-' if chars.peek() == Some(&'-') => {
                // Line comment: consume to end of line (keep the newline so
                // GO-separator detection still sees line boundaries).
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                // A comment acts as whitespace between tokens.
                out.push(' ');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Split a batch on `;` and standalone `GO` lines. String literals are
/// respected (a `;` inside quotes does not split), and SQL comments are
/// stripped first so a trailing `-- note` never reads as a statement.
pub fn split_statements(sql: &str) -> Vec<String> {
    let sql = strip_comments(sql);
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for ch in sql.chars() {
        match ch {
            '\'' => {
                in_string = !in_string;
                current.push(ch);
            }
            ';' if !in_string => {
                statements.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    statements.push(current);

    // Second pass: split each piece on standalone GO lines.
    let mut out = Vec::new();
    for piece in statements {
        let mut buf = String::new();
        for line in piece.lines() {
            if line.trim().eq_ignore_ascii_case("go") {
                out.push(std::mem::take(&mut buf));
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        out.push(buf);
    }
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// What a SELECT projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    All,
    Columns(Vec<String>),
    CountStar,
}

/// Comparison operators supported in `WHERE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
}

/// A `WHERE column op literal` condition.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub column: String,
    pub op: Op,
    pub value: Value,
}

impl Condition {
    /// Whether `cell` satisfies this condition.
    pub fn matches(&self, cell: &Value) -> bool {
        use std::cmp::Ordering;
        // LIKE: '%' wildcards on text, case-insensitive.
        if self.op == Op::Like {
            let (Value::Text(pattern), Value::Text(cell_text)) = (&self.value, cell) else {
                return false;
            };
            return like_match(&pattern.to_lowercase(), &cell_text.to_lowercase());
        }
        let ord = match (cell, &self.value) {
            (Value::Null, _) | (_, Value::Null) => return false,
            (Value::Text(a), Value::Text(b)) => Some(a.to_lowercase().cmp(&b.to_lowercase())),
            (a, b) => {
                let num = |v: &Value| -> Option<f64> {
                    match v {
                        Value::Int(i) => Some(*i as f64),
                        Value::Float(f) => Some(*f),
                        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                        _ => None,
                    }
                };
                match (num(a), num(b)) {
                    (Some(x), Some(y)) => x.partial_cmp(&y),
                    _ => None,
                }
            }
        };
        let Some(ord) = ord else { return false };
        match self.op {
            Op::Eq => ord == Ordering::Equal,
            Op::Ne => ord != Ordering::Equal,
            Op::Lt => ord == Ordering::Less,
            Op::Le => ord != Ordering::Greater,
            Op::Gt => ord == Ordering::Greater,
            Op::Ge => ord != Ordering::Less,
            Op::Like => unreachable!("handled above"),
        }
    }
}

/// `%`-wildcard match (both inputs already lowercased).
fn like_match(pattern: &str, text: &str) -> bool {
    // Convert the LIKE pattern into segments split by '%': the text must
    // contain the segments in order, anchored at the ends when the pattern
    // does not begin/end with '%'.
    let segments: Vec<&str> = pattern.split('%').collect();
    let mut pos = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(seg) {
                return false;
            }
            pos = seg.len();
        } else if i == segments.len() - 1 {
            return text[pos..].ends_with(seg);
        } else {
            match text[pos..].find(seg) {
                Some(found) => pos += found + seg.len(),
                None => return false,
            }
        }
    }
    true
}

/// `ORDER BY column [ASC|DESC]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

/// A parsed SELECT.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectQuery {
    pub projection: Projection,
    pub from: String,
    pub where_clause: Option<Condition>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}

/// Tokenize: words, quoted strings (kept as one token with quotes), and
/// punctuation (`,`, `(`, `)`, comparison operators).
fn tokenize(sql: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
        } else if ch == '\'' {
            let mut s = String::from(chars.next().unwrap());
            for c in chars.by_ref() {
                s.push(c);
                if c == '\'' {
                    break;
                }
            }
            tokens.push(s);
        } else if ch == ',' || ch == '(' || ch == ')' {
            tokens.push(chars.next().unwrap().to_string());
        } else if ch == '=' || ch == '<' || ch == '>' || ch == '!' {
            let mut s = String::from(chars.next().unwrap());
            if let Some(&next) = chars.peek()
                && (next == '=' || next == '>')
            {
                s.push(chars.next().unwrap());
            }
            tokens.push(s);
        } else {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace()
                    || c == ','
                    || c == '('
                    || c == ')'
                    || c == '='
                    || c == '<'
                    || c == '>'
                    || c == '!'
                {
                    break;
                }
                s.push(chars.next().unwrap());
            }
            tokens.push(s);
        }
    }
    tokens
}

/// Strip `[brackets]` / `"quotes"` and any `schema.` prefix from an
/// identifier.
fn clean_ident(raw: &str) -> String {
    let raw = raw
        .trim()
        .trim_matches(|c| c == '[' || c == ']' || c == '"');
    match raw.rsplit_once('.') {
        Some((_, name)) => name
            .trim_matches(|c| c == '[' || c == ']' || c == '"')
            .to_string(),
        None => raw.to_string(),
    }
}

fn parse_literal(token: &str) -> Value {
    if token.starts_with('\'') {
        Value::Text(token.trim_matches('\'').to_string())
    } else if token.eq_ignore_ascii_case("null") {
        Value::Null
    } else if let Ok(i) = token.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = token.parse::<f64>() {
        Value::Float(f)
    } else {
        Value::Text(token.to_string())
    }
}

/// Parse a supported `SELECT`.
pub fn parse_select(sql: &str) -> Result<SelectQuery, FakeDbError> {
    let tokens = tokenize(sql);
    let mut i = 0usize;
    let word = |i: usize| -> String { tokens.get(i).map(|t| t.to_uppercase()).unwrap_or_default() };

    if word(i) != "SELECT" {
        return Err(FakeDbError("Expected SELECT".to_string()));
    }
    i += 1;

    // TOP n
    let mut limit: Option<usize> = None;
    if word(i) == "TOP" {
        let n = tokens
            .get(i + 1)
            .and_then(|t| t.parse::<usize>().ok())
            .ok_or_else(|| FakeDbError("TOP requires a number".to_string()))?;
        limit = Some(n);
        i += 2;
    }

    // Projection up to FROM
    let mut projection_tokens: Vec<String> = Vec::new();
    while i < tokens.len() && word(i) != "FROM" {
        projection_tokens.push(tokens[i].clone());
        i += 1;
    }
    if word(i) != "FROM" {
        return Err(FakeDbError(
            "Expected FROM <table or view> in SELECT".to_string(),
        ));
    }
    i += 1;

    let projection = if projection_tokens.len() == 1 && projection_tokens[0] == "*" {
        Projection::All
    } else if projection_tokens
        .iter()
        .map(|t| t.to_uppercase())
        .collect::<Vec<_>>()
        .join("")
        == "COUNT(*)"
    {
        Projection::CountStar
    } else {
        let names: Vec<String> = projection_tokens
            .iter()
            .filter(|t| *t != ",")
            .map(|t| clean_ident(t))
            .collect();
        if names.is_empty() {
            return Err(FakeDbError("SELECT needs a projection".to_string()));
        }
        Projection::Columns(names)
    };

    let from = clean_ident(
        tokens
            .get(i)
            .ok_or_else(|| FakeDbError("FROM requires a table or view name".to_string()))?,
    );
    i += 1;

    let mut where_clause = None;
    let mut order_by = None;

    while i < tokens.len() {
        match word(i).as_str() {
            "WHERE" => {
                let column = clean_ident(
                    tokens
                        .get(i + 1)
                        .ok_or_else(|| FakeDbError("WHERE requires a column".to_string()))?,
                );
                let op = match tokens
                    .get(i + 2)
                    .map(|t| t.to_uppercase())
                    .unwrap_or_default()
                    .as_str()
                {
                    "=" => Op::Eq,
                    "!=" | "<>" => Op::Ne,
                    "<" => Op::Lt,
                    "<=" => Op::Le,
                    ">" => Op::Gt,
                    ">=" => Op::Ge,
                    "LIKE" => Op::Like,
                    other => {
                        return Err(FakeDbError(format!(
                            "Unsupported WHERE operator '{other}'. \
                             Supported: = != <> < <= > >= LIKE"
                        )));
                    }
                };
                let value =
                    parse_literal(tokens.get(i + 3).ok_or_else(|| {
                        FakeDbError("WHERE requires a literal value".to_string())
                    })?);
                where_clause = Some(Condition { column, op, value });
                i += 4;
            }
            "ORDER" => {
                if word(i + 1) != "BY" {
                    return Err(FakeDbError("Expected ORDER BY".to_string()));
                }
                let column = clean_ident(
                    tokens
                        .get(i + 2)
                        .ok_or_else(|| FakeDbError("ORDER BY requires a column".to_string()))?,
                );
                let mut descending = false;
                i += 3;
                match word(i).as_str() {
                    "DESC" => {
                        descending = true;
                        i += 1;
                    }
                    "ASC" => {
                        i += 1;
                    }
                    _ => {}
                }
                order_by = Some(OrderBy { column, descending });
            }
            "LIMIT" => {
                let n = tokens
                    .get(i + 1)
                    .and_then(|t| t.parse::<usize>().ok())
                    .ok_or_else(|| FakeDbError("LIMIT requires a number".to_string()))?;
                limit = Some(n);
                i += 2;
            }
            other => {
                return Err(FakeDbError(format!(
                    "Unsupported SQL near '{other}'. The demo database supports \
                     SELECT [TOP n] cols FROM name [WHERE col op value] \
                     [ORDER BY col [DESC]] [LIMIT n]."
                )));
            }
        }
    }

    Ok(SelectQuery {
        projection,
        from,
        where_clause,
        order_by,
        limit,
    })
}

/// A parsed INSERT: target table, optional column list, and literal rows.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub table: String,
    /// Empty means positional (all columns, in table order).
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// A parsed UPDATE: target, SET assignments, optional WHERE.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    pub table: String,
    pub assignments: Vec<(String, Value)>,
    pub where_clause: Option<Condition>,
}

/// A parsed DELETE: target and optional WHERE.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub table: String,
    pub where_clause: Option<Condition>,
}

/// A parsed CREATE TABLE: name plus `(column type)` pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStatement {
    pub table: String,
    pub columns: Vec<(String, String)>,
}

/// Parse the `WHERE col op literal` tail shared by UPDATE and DELETE.
/// Returns the condition and how many tokens it consumed (0 when absent).
fn parse_where_tail(
    tokens: &[String],
    i: usize,
) -> Result<(Option<Condition>, usize), FakeDbError> {
    if tokens.get(i).map(|t| t.to_uppercase()).as_deref() != Some("WHERE") {
        return Ok((None, 0));
    }
    let column = clean_ident(
        tokens
            .get(i + 1)
            .ok_or_else(|| FakeDbError("WHERE requires a column".to_string()))?,
    );
    let op = match tokens
        .get(i + 2)
        .map(|t| t.to_uppercase())
        .unwrap_or_default()
        .as_str()
    {
        "=" => Op::Eq,
        "!=" | "<>" => Op::Ne,
        "<" => Op::Lt,
        "<=" => Op::Le,
        ">" => Op::Gt,
        ">=" => Op::Ge,
        "LIKE" => Op::Like,
        other => {
            return Err(FakeDbError(format!(
                "Unsupported WHERE operator '{other}'. Supported: = != <> < <= > >= LIKE"
            )));
        }
    };
    let value = parse_literal(
        tokens
            .get(i + 3)
            .ok_or_else(|| FakeDbError("WHERE requires a literal value".to_string()))?,
    );
    Ok((Some(Condition { column, op, value }), 4))
}

/// Parse `INSERT INTO name [(col, ...)] VALUES (lit, ...) [, (lit, ...)]`.
pub fn parse_insert(sql: &str) -> Result<InsertStatement, FakeDbError> {
    let tokens = tokenize(sql);
    let word = |i: usize| -> String { tokens.get(i).map(|t| t.to_uppercase()).unwrap_or_default() };
    let mut i = 0usize;
    if word(i) != "INSERT" {
        return Err(FakeDbError("Expected INSERT".to_string()));
    }
    i += 1;
    if word(i) == "INTO" {
        i += 1;
    }
    let table = clean_ident(
        tokens
            .get(i)
            .ok_or_else(|| FakeDbError("INSERT requires a table name".to_string()))?,
    );
    i += 1;

    let mut columns = Vec::new();
    if tokens.get(i).map(String::as_str) == Some("(") {
        i += 1;
        while i < tokens.len() && tokens[i] != ")" {
            if tokens[i] != "," {
                columns.push(clean_ident(&tokens[i]));
            }
            i += 1;
        }
        if tokens.get(i).map(String::as_str) != Some(")") {
            return Err(FakeDbError("Unclosed column list in INSERT".to_string()));
        }
        i += 1;
    }

    if word(i) != "VALUES" {
        return Err(FakeDbError(
            "INSERT requires a VALUES (...) clause".to_string(),
        ));
    }
    i += 1;

    let mut rows = Vec::new();
    loop {
        if tokens.get(i).map(String::as_str) != Some("(") {
            return Err(FakeDbError(
                "VALUES requires a parenthesized row".to_string(),
            ));
        }
        i += 1;
        let mut row = Vec::new();
        while i < tokens.len() && tokens[i] != ")" {
            if tokens[i] != "," {
                row.push(parse_literal(&tokens[i]));
            }
            i += 1;
        }
        if tokens.get(i).map(String::as_str) != Some(")") {
            return Err(FakeDbError("Unclosed VALUES row".to_string()));
        }
        i += 1;
        rows.push(row);
        if tokens.get(i).map(String::as_str) == Some(",") {
            i += 1;
            continue;
        }
        break;
    }
    if i < tokens.len() {
        return Err(FakeDbError(format!(
            "Unsupported SQL after VALUES near '{}'",
            tokens[i]
        )));
    }
    Ok(InsertStatement {
        table,
        columns,
        rows,
    })
}

/// Parse `UPDATE name SET col = lit [, col = lit ...] [WHERE col op lit]`.
pub fn parse_update(sql: &str) -> Result<UpdateStatement, FakeDbError> {
    let tokens = tokenize(sql);
    let word = |i: usize| -> String { tokens.get(i).map(|t| t.to_uppercase()).unwrap_or_default() };
    let mut i = 0usize;
    if word(i) != "UPDATE" {
        return Err(FakeDbError("Expected UPDATE".to_string()));
    }
    let table = clean_ident(
        tokens
            .get(i + 1)
            .ok_or_else(|| FakeDbError("UPDATE requires a table name".to_string()))?,
    );
    i += 2;
    if word(i) != "SET" {
        return Err(FakeDbError("UPDATE requires a SET clause".to_string()));
    }
    i += 1;

    let mut assignments = Vec::new();
    loop {
        let column = clean_ident(
            tokens
                .get(i)
                .ok_or_else(|| FakeDbError("SET requires a column".to_string()))?,
        );
        if tokens.get(i + 1).map(String::as_str) != Some("=") {
            return Err(FakeDbError(format!("SET {column} requires '='")));
        }
        let value = parse_literal(
            tokens
                .get(i + 2)
                .ok_or_else(|| FakeDbError(format!("SET {column} requires a literal value")))?,
        );
        assignments.push((column, value));
        i += 3;
        if tokens.get(i).map(String::as_str) == Some(",") {
            i += 1;
            continue;
        }
        break;
    }

    let (where_clause, consumed) = parse_where_tail(&tokens, i)?;
    i += consumed;
    if i < tokens.len() {
        return Err(FakeDbError(format!(
            "Unsupported SQL in UPDATE near '{}'",
            tokens[i]
        )));
    }
    Ok(UpdateStatement {
        table,
        assignments,
        where_clause,
    })
}

/// Parse `DELETE [FROM] name [WHERE col op lit]`.
pub fn parse_delete(sql: &str) -> Result<DeleteStatement, FakeDbError> {
    let tokens = tokenize(sql);
    let word = |i: usize| -> String { tokens.get(i).map(|t| t.to_uppercase()).unwrap_or_default() };
    let mut i = 0usize;
    if word(i) != "DELETE" {
        return Err(FakeDbError("Expected DELETE".to_string()));
    }
    i += 1;
    if word(i) == "FROM" {
        i += 1;
    }
    let table = clean_ident(
        tokens
            .get(i)
            .ok_or_else(|| FakeDbError("DELETE requires a table name".to_string()))?,
    );
    i += 1;
    let (where_clause, consumed) = parse_where_tail(&tokens, i)?;
    i += consumed;
    if i < tokens.len() {
        return Err(FakeDbError(format!(
            "Unsupported SQL in DELETE near '{}'",
            tokens[i]
        )));
    }
    Ok(DeleteStatement {
        table,
        where_clause,
    })
}

/// Parse `CREATE TABLE name (col type [more words ignored], ...)`. Constraint
/// keywords after the type are tolerated and ignored — the demo store keeps
/// no constraints.
pub fn parse_create_table(sql: &str) -> Result<CreateTableStatement, FakeDbError> {
    let tokens = tokenize(sql);
    let word = |i: usize| -> String { tokens.get(i).map(|t| t.to_uppercase()).unwrap_or_default() };
    let mut i = 0usize;
    if word(i) != "CREATE" || word(i + 1) != "TABLE" {
        return Err(FakeDbError("Expected CREATE TABLE".to_string()));
    }
    let table = clean_ident(
        tokens
            .get(i + 2)
            .ok_or_else(|| FakeDbError("CREATE TABLE requires a name".to_string()))?,
    );
    i += 3;
    if tokens.get(i).map(String::as_str) != Some("(") {
        return Err(FakeDbError(
            "CREATE TABLE requires a (column type, ...) list".to_string(),
        ));
    }
    i += 1;

    let mut columns = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut depth = 1u32;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "(" => {
                depth += 1;
                current.push(tokens[i].clone());
            }
            ")" => {
                depth -= 1;
                if depth == 0 {
                    if !current.is_empty() {
                        columns.push(column_def_from(&current)?);
                    }
                    break;
                }
                current.push(tokens[i].clone());
            }
            "," if depth == 1 => {
                if !current.is_empty() {
                    columns.push(column_def_from(&current)?);
                    current.clear();
                }
            }
            _ => current.push(tokens[i].clone()),
        }
        i += 1;
    }
    if depth != 0 {
        return Err(FakeDbError("Unclosed CREATE TABLE column list".to_string()));
    }
    if columns.is_empty() {
        return Err(FakeDbError(
            "CREATE TABLE requires at least one column".to_string(),
        ));
    }
    Ok(CreateTableStatement { table, columns })
}

fn column_def_from(tokens: &[String]) -> Result<(String, String), FakeDbError> {
    let name = tokens
        .first()
        .map(|t| clean_ident(t))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| FakeDbError("Column definition requires a name".to_string()))?;
    let type_name = tokens.get(1).cloned().unwrap_or_else(|| "text".to_string());
    Ok((name, type_name))
}

/// Parse `EXEC[UTE] name ['arg', 2, ...]` into the procedure name + args.
pub fn parse_exec(sql: &str) -> Result<(String, Vec<Value>), FakeDbError> {
    let tokens = tokenize(sql);
    if tokens.is_empty()
        || !(tokens[0].eq_ignore_ascii_case("exec") || tokens[0].eq_ignore_ascii_case("execute"))
    {
        return Err(FakeDbError("Expected EXEC <procedure>".to_string()));
    }
    let name = clean_ident(
        tokens
            .get(1)
            .ok_or_else(|| FakeDbError("EXEC requires a procedure name".to_string()))?,
    );
    let args = tokens[2..]
        .iter()
        .filter(|t| *t != ",")
        .map(|t| parse_literal(t))
        .collect();
    Ok((name, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_semicolons_and_go() {
        let stmts = split_statements("SELECT 1; SELECT 2\nGO\nSELECT 3");
        assert_eq!(stmts.len(), 3);
    }

    #[test]
    fn semicolon_inside_string_does_not_split() {
        let stmts = split_statements("SELECT * FROM T WHERE a = 'x;y'");
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn line_comments_are_stripped_not_executed() {
        let stmts =
            split_statements("SELECT * FROM Customers;\n-- try: EXEC TopProductsByPrice 3;");
        assert_eq!(stmts, vec!["SELECT * FROM Customers".to_string()]);
    }

    #[test]
    fn block_comments_are_stripped() {
        let stmts = split_statements("SELECT * /* all columns */ FROM T");
        assert_eq!(stmts.len(), 1);
        assert!(!stmts[0].contains("/*"));
    }

    #[test]
    fn comment_markers_inside_strings_survive() {
        let stmts = split_statements("SELECT * FROM T WHERE a = '--not a comment'");
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("--not a comment"));
    }

    #[test]
    fn parses_full_select() {
        let q = parse_select(
            "SELECT TOP 5 Name, Price FROM Products WHERE Price >= 10 ORDER BY Price DESC",
        )
        .unwrap();
        assert_eq!(q.limit, Some(5));
        assert_eq!(
            q.projection,
            Projection::Columns(vec!["Name".into(), "Price".into()])
        );
        assert_eq!(q.from, "Products");
        let w = q.where_clause.unwrap();
        assert_eq!(w.op, Op::Ge);
        assert_eq!(w.value, Value::Int(10));
        let o = q.order_by.unwrap();
        assert!(o.descending);
    }

    #[test]
    fn parses_bracketed_and_schema_qualified_names() {
        let q = parse_select("SELECT * FROM [dbo].[Products]").unwrap();
        assert_eq!(q.from, "Products");
    }

    #[test]
    fn parses_count_star() {
        let q = parse_select("SELECT COUNT ( * ) FROM T").unwrap();
        assert_eq!(q.projection, Projection::CountStar);
    }

    #[test]
    fn parses_exec_with_args() {
        let (name, args) = parse_exec("EXEC CustomerOrders 'ALFKI', 3").unwrap();
        assert_eq!(name, "CustomerOrders");
        assert_eq!(args, vec![Value::Text("ALFKI".into()), Value::Int(3)]);
    }

    #[test]
    fn like_matching() {
        let cond = Condition {
            column: "a".into(),
            op: Op::Like,
            value: Value::Text("%chai%".into()),
        };
        assert!(cond.matches(&Value::Text("Chai Tea".into())));
        assert!(!cond.matches(&Value::Text("Coffee".into())));
        let anchored = Condition {
            column: "a".into(),
            op: Op::Like,
            value: Value::Text("cha%".into()),
        };
        assert!(anchored.matches(&Value::Text("Chai".into())));
        assert!(!anchored.matches(&Value::Text("Matcha".into())));
    }

    #[test]
    fn unsupported_operator_reports_helpfully() {
        let err = parse_select("SELECT * FROM T WHERE a IN (1)").unwrap_err();
        assert!(err.0.contains("Unsupported WHERE operator"));
    }
}
