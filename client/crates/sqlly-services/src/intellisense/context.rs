//! Shared completion context builder. Ports Swift's
//! `EditorIntelliSenseContext.swift` — the context that both the policy
//! evaluator and the completion pipeline consume.
//!
//! The context is built from the SQL text + cursor position + trigger kind and
//! carries: prefix, replacement span, clause, intent, qualifier, table refs,
//! aliases, parameters, and value-context detection. It is allocation-conscious:
//! statement isolation and prefix extraction are single backward scans.

use crate::intellisense::rules::{IntelliSenseClause, IntelliSenseTriggerKind};
use sqlly_intel::intellisense::{current_prefix, statement_range};
use sqlly_lang::sqlmodel::Dialect;

/// A table reference extracted from FROM/JOIN clauses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

/// The comparison operator that introduces a value-literal position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueOperator {
    Equals,
    Like,
}

/// When the cursor sits in a position where a literal value is expected
/// (after `=` or `LIKE`), this captures the surrounding context: the column,
/// optional table/schema qualifier, the operator, the partial value the user
/// has typed so far, the byte span that should be replaced when a completion
/// is accepted, and whether the user opened a single-quoted literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueContext {
    pub column: Option<String>,
    pub table: Option<String>,
    pub schema: Option<String>,
    pub operator: ValueOperator,
    pub partial_value: String,
    pub replace_start: usize,
    pub replace_end: usize,
    pub has_opening_quote: bool,
}

/// The full completion context, porting Swift's `IntelliSenseContext`.
/// Built from the SQL text + cursor position + trigger kind.
#[derive(Debug, Clone)]
pub struct CompletionContext {
    /// The raw prefix at the cursor (e.g. `dbo.Ord`).
    pub prefix: String,
    /// The replacement text — the part after the last dot (e.g. `Ord`).
    pub replace_text: String,
    /// Byte offset where replacement should start.
    pub replace_start: usize,
    /// Byte offset where replacement should end (cursor position).
    pub replace_end: usize,
    /// What triggered the completion request.
    pub trigger: IntelliSenseTriggerKind,
    /// Whether this was a manual trigger (Ctrl+Space etc.).
    pub manual: bool,
    /// The SQL clause at the cursor position.
    pub clause: IntelliSenseClause,
    /// The completion intent derived from clause + prefix + qualifier.
    pub intent: IntelliSenseIntent,
    /// Whether the prefix contains a dot qualifier.
    pub is_qualified_access: bool,
    /// The qualifier portion of the prefix (before the last dot).
    pub qualifier: Option<String>,
    /// Whether there are table aliases in scope.
    pub has_aliases: bool,
    /// Number of alias references.
    pub alias_count: i32,
    /// Number of table references in the statement.
    pub table_count: i32,
    /// Table references extracted from FROM/JOIN.
    pub table_refs: Vec<TableRef>,
    /// Aliases as (alias_name, table_name) pairs.
    pub aliases: Vec<(String, String)>,
    /// @-parameters found in the SQL before the cursor.
    pub parameters: Vec<String>,
    /// Value context if the cursor is in a value-literal position.
    pub value_context: Option<ValueContext>,
    /// Lowercased current-statement text (for statement_contains checks).
    pub statement_lower: String,
    /// The caret follows a JOIN source and its alias with no `ON` written
    /// yet: join conditions are offered with the `ON` keyword included, and
    /// nothing else (feedback ifb_e275).
    pub join_on_missing: bool,
}

impl CompletionContext {
    /// Build the context from the SQL text + cursor + trigger.
    ///
    /// `parameters` can be passed in from editor state (declared variables,
    /// procedure parameters). If empty, @-parameters are extracted from the SQL.
    pub fn build(
        sql: &str,
        cursor: usize,
        trigger: IntelliSenseTriggerKind,
        manual: bool,
        parameters: &[String],
    ) -> Self {
        let cursor = cursor.min(sql.len());
        let prefix = current_prefix(sql, cursor).to_string();
        let dot_idx = prefix.rfind('.');
        let replace_text = match dot_idx {
            Some(idx) => prefix[idx + 1..].to_string(),
            None => prefix.clone(),
        };
        let qualifier = dot_idx.map(|idx| prefix[..idx].to_string());
        let is_qualified_access = qualifier.is_some();

        let replace_start = cursor - replace_text.len();
        let replace_end = cursor;

        let statement_prefix = statement_prefix_before_cursor(sql, cursor);
        let mut clause = detect_clause(&statement_prefix);
        let mut join_on_missing = false;
        if clause == IntelliSenseClause::Join {
            // `JOIN T t t.Col = ▏` — the condition written without its `ON`
            // is still a join condition, not a literal value (feedback
            // ifb_e85a).
            if sqlly_intel::intellisense::join_condition_without_on(sql, cursor) {
                clause = IntelliSenseClause::On;
            } else if sqlly_intel::intellisense::join_awaiting_on(sql, cursor) {
                // `JOIN T t ▏` — the joined table still needs its condition
                // before anything else can follow (feedback ifb_e275).
                clause = IntelliSenseClause::On;
                join_on_missing = true;
            }
        }
        let aliases = sqlly_intel::intellisense::query_aliases(sql, cursor);
        let has_aliases = !aliases.is_empty();
        let alias_count = aliases.len() as i32;
        let table_refs = extract_table_refs(&statement_prefix);
        let table_count = table_refs.len() as i32;

        let params = if parameters.is_empty() {
            extract_parameters(sql, cursor)
        } else {
            parameters.to_vec()
        };

        let value_context = if clause != IntelliSenseClause::On {
            detect_value_context(sql, cursor).map(|vc| resolve_value_context_table(vc, &table_refs))
        } else {
            None
        };

        let intent = detect_intent(
            &prefix,
            qualifier.as_deref(),
            clause,
            value_context.as_ref(),
            sql,
            cursor,
        );

        let statement_lower = statement_prefix.to_ascii_lowercase();

        Self {
            prefix,
            replace_text,
            replace_start,
            replace_end,
            trigger,
            manual,
            clause,
            intent,
            is_qualified_access,
            qualifier,
            has_aliases,
            alias_count,
            table_count,
            table_refs,
            aliases,
            parameters: params,
            value_context,
            statement_lower,
            join_on_missing,
        }
    }

    /// Convenience: build with `AutomaticTyping` trigger, non-manual.
    pub fn auto(sql: &str, cursor: usize) -> Self {
        Self::build(
            sql,
            cursor,
            IntelliSenseTriggerKind::AutomaticTyping,
            false,
            &[],
        )
    }
}

// ---------------------------------------------------------------------------
// Intent — porting Swift's `intentFor`
// ---------------------------------------------------------------------------

use crate::intellisense::rules::IntelliSenseIntent;

fn detect_intent(
    prefix: &str,
    qualifier: Option<&str>,
    clause: IntelliSenseClause,
    value_context: Option<&ValueContext>,
    sql: &str,
    cursor: usize,
) -> IntelliSenseIntent {
    // EXEC clause takes priority — even with a dotted prefix like `dbo.Get`,
    // the user wants procedure completions, not generic qualified-member
    // column/table completions.
    if clause == IntelliSenseClause::Exec {
        return IntelliSenseIntent::ExecProcedure;
    }
    if qualifier.is_some() {
        return IntelliSenseIntent::QualifiedMember;
    }
    if value_context.is_some() {
        return IntelliSenseIntent::ValueLiteral;
    }
    if prefix.starts_with('@') {
        return IntelliSenseIntent::Parameter;
    }

    // Keywords are matched case-insensitively: `update ▏` is an UPDATE head
    // whatever its case (feedback ifb_3798).
    let prev = previous_keyword(sql, cursor).to_ascii_uppercase();
    match prev.as_str() {
        // The object-name positions, including the head of a fresh
        // INSERT/UPDATE/DELETE/MERGE statement (feedback ifb_3798).
        "FROM" | "INTO" | "UPDATE" | "TABLE" | "APPLY" | "INSERT" | "DELETE" | "MERGE"
        | "USING" => return IntelliSenseIntent::FromSource,
        "JOIN" => return IntelliSenseIntent::JoinSource,
        "ON" => return IntelliSenseIntent::JoinCondition,
        // `DROP TABLE IF EXISTS ▏` / `DROP VIEW ▏` name an object to drop
        // (feedback ifb_416b); `EXISTS` elsewhere opens a subquery, which
        // the parenthesis makes a fresh clause.
        "EXISTS" | "VIEW" | "FUNCTION" if clause == IntelliSenseClause::Drop => {
            return IntelliSenseIntent::FromSource;
        }
        "PROCEDURE" | "PROC" if clause == IntelliSenseClause::Drop => {
            return IntelliSenseIntent::ExecProcedure;
        }
        "WHERE" | "AND" | "OR" | "WHEN" => return IntelliSenseIntent::WherePredicate,
        "GROUP" | "BY" => {
            return if clause == IntelliSenseClause::OrderBy {
                IntelliSenseIntent::OrderBy
            } else {
                IntelliSenseIntent::GroupBy
            };
        }
        "ORDER" => return IntelliSenseIntent::OrderBy,
        "HAVING" => return IntelliSenseIntent::Having,
        "SET" => return IntelliSenseIntent::SetAssignment,
        "SELECT" | "DISTINCT" | "TOP" => return IntelliSenseIntent::SelectList,
        _ => {}
    }

    match clause {
        IntelliSenseClause::Select => IntelliSenseIntent::SelectList,
        IntelliSenseClause::From => IntelliSenseIntent::FromSource,
        IntelliSenseClause::Where => IntelliSenseIntent::WherePredicate,
        IntelliSenseClause::Join => IntelliSenseIntent::JoinSource,
        IntelliSenseClause::On => IntelliSenseIntent::JoinCondition,
        IntelliSenseClause::GroupBy => IntelliSenseIntent::GroupBy,
        IntelliSenseClause::OrderBy => IntelliSenseIntent::OrderBy,
        IntelliSenseClause::Having => IntelliSenseIntent::Having,
        IntelliSenseClause::Set => IntelliSenseIntent::SetAssignment,
        IntelliSenseClause::Values => IntelliSenseIntent::ValueLiteral,
        IntelliSenseClause::Exec => IntelliSenseIntent::ExecProcedure,
        IntelliSenseClause::Drop => IntelliSenseIntent::FromSource,
        IntelliSenseClause::Unknown => IntelliSenseIntent::General,
        _ => IntelliSenseIntent::General,
    }
}

// ---------------------------------------------------------------------------
// Clause detection — statement-scoped
// ---------------------------------------------------------------------------

fn detect_clause(statement_prefix: &str) -> IntelliSenseClause {
    let upper = statement_prefix.to_ascii_uppercase();
    let words: Vec<&str> = upper
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut idx = words.len();
    while idx > 0 {
        idx -= 1;
        match words[idx] {
            "SELECT" => return IntelliSenseClause::Select,
            "FROM" => return IntelliSenseClause::From,
            "WHERE" => return IntelliSenseClause::Where,
            "JOIN" | "INNER" | "LEFT" | "RIGHT" | "OUTER" | "CROSS" | "FULL" => {
                return IntelliSenseClause::Join;
            }
            "ON" => return IntelliSenseClause::On,
            "HAVING" => return IntelliSenseClause::Having,
            "SET" => return IntelliSenseClause::Set,
            "VALUES" => return IntelliSenseClause::Values,
            "EXEC" | "EXECUTE" => return IntelliSenseClause::Exec,
            "INSERT" => return IntelliSenseClause::Insert,
            "UPDATE" => return IntelliSenseClause::Update,
            "DELETE" => return IntelliSenseClause::Delete,
            "CREATE" => return IntelliSenseClause::Create,
            "ALTER" => return IntelliSenseClause::Alter,
            "DROP" => return IntelliSenseClause::Drop,
            "BY" if idx > 0 => {
                let prev = words[idx - 1];
                if prev == "GROUP" {
                    return IntelliSenseClause::GroupBy;
                }
                if prev == "ORDER" {
                    return IntelliSenseClause::OrderBy;
                }
            }
            _ => {}
        }
    }
    IntelliSenseClause::Unknown
}

// ---------------------------------------------------------------------------
// Statement isolation — the caret's statement, up to the cursor
// ---------------------------------------------------------------------------

/// The text of the statement the caret is in, from its start to the cursor.
/// Bounded by the shared statement splitter (`;`, `GO`, and a statement
/// keyword opening a new statement inside an unterminated one), so a finished
/// `SELECT … FROM t AS x` followed by `update ▏` on a new line yields just
/// `update ` (feedback ifb_3798).
fn statement_prefix_before_cursor(sql: &str, cursor: usize) -> String {
    let cursor = cursor.min(sql.len());
    let range = statement_range(sql, cursor, Dialect::SqlServer);
    sql[range.start.min(cursor)..cursor].to_string()
}

// ---------------------------------------------------------------------------
// Previous keyword — scan backward past the current word to the keyword before
// ---------------------------------------------------------------------------

fn previous_keyword(sql: &str, cursor: usize) -> &str {
    let cursor = cursor.min(sql.len());
    let upto = &sql[..cursor];
    let is_token_char =
        |ch: char| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '@' || ch == '#';
    let before = upto.trim_end();
    // With the caret on a word (`FROM Ord▏`) the keyword is the word before
    // it; once a separator follows the word (`UPDATE ▏`) that word itself is
    // the keyword — skipping it read `UPDATE ▏` as "after the alias before
    // UPDATE" (feedback ifb_3798).
    let before_prev = if upto.ends_with(is_token_char) {
        let end = before
            .rfind(|ch: char| !is_token_char(ch))
            .unwrap_or(before.len());
        before[..end].trim_end()
    } else {
        before
    };
    let start = before_prev
        .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        // `rfind` returns the separator's *start* byte; advancing by 1 would
        // land inside a multi-byte char (e.g. a Spanish `ó`) and panic when
        // sliced. Step past the separator's full UTF-8 width instead.
        .map_or(0, |idx| {
            idx + before_prev[idx..].chars().next().map_or(1, char::len_utf8)
        });
    &before_prev[start..]
}

// ---------------------------------------------------------------------------
// Value context detection — ports Swift's `EditorCompletion.valueContext`
// ---------------------------------------------------------------------------

/// Detect whether the cursor sits in a value-literal position (after `=` or
/// `LIKE` in a WHERE/HAVING/SET clause) and, if so, capture the full context:
/// the column, optional table/schema qualifier, the operator, the partial
/// value typed so far, the byte span to replace on accept, and whether the
/// user opened a single-quoted literal.
fn detect_value_context(sql: &str, cursor: usize) -> Option<ValueContext> {
    let cursor = cursor.min(sql.len());
    let bytes = sql.as_bytes();
    let mut idx = cursor;

    // Phase 1: Skip trailing whitespace to find the end of any partial value.
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    let text_end = idx;
    // Phase 2: Consume partial value (if any) and track opening quote.
    let mut value_start = cursor; // Default: insertion at cursor, no value to replace.
    let mut has_opening_quote = false;

    if idx > 0 && bytes[idx - 1] == b'=' {
        // Cursor right after `=` (with optional whitespace) — no partial value.
    } else if idx > 0 {
        if bytes[idx - 1] == b'\'' {
            // Quoted literal at cursor. Determine opening-quote state.
            let (vs, hoq) = consume_quoted_value_backward(bytes, idx);
            value_start = vs;
            has_opening_quote = hoq;
        } else {
            // Unquoted partial value — consume backward.
            let consumed_end = idx;
            while idx > 0 {
                let b = bytes[idx - 1];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-' {
                    idx -= 1;
                } else {
                    break;
                }
            }
            let consumed = &sql[idx..consumed_end];

            if consumed.eq_ignore_ascii_case("LIKE") {
                // The "partial value" was actually the LIKE keyword — no value.
                // value_start stays at cursor.
            } else {
                value_start = idx;
                // Check if there's an opening quote just before the consumed value.
                if idx > 0 && bytes[idx - 1] == b'\'' {
                    let mut before = idx - 1;
                    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
                        before -= 1;
                    }
                    if before > 0
                        && (bytes[before - 1] == b'=' || is_like_keyword_end(bytes, before))
                    {
                        has_opening_quote = true;
                        value_start = idx - 1;
                    }
                }
            }
        }
    }

    // Extract partial value (without opening quote).
    let partial_value = if value_start < text_end {
        let raw = &sql[value_start..text_end];
        if has_opening_quote && raw.starts_with('\'') {
            raw[1..].to_string()
        } else {
            raw.to_string()
        }
    } else {
        String::new()
    };

    // Phase 3: Skip whitespace to operator position.
    idx = value_start.min(text_end);
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }

    // Phase 4: Detect operator — `=` (reject compound) or `LIKE` keyword.
    let operator = if idx > 0 && bytes[idx - 1] == b'=' {
        // Reject compound operators: <=, >=, !=, <>
        if idx >= 2 {
            let prev = bytes[idx - 2];
            if prev == b'<' || prev == b'>' || prev == b'!' {
                return None;
            }
        }
        idx -= 1; // Move past the `=` to find the column before it.
        ValueOperator::Equals
    } else {
        // Check for LIKE keyword.
        let kw_end = idx;
        while idx > 0 {
            let b = bytes[idx - 1];
            if b.is_ascii_alphanumeric() || b == b'_' {
                idx -= 1;
            } else {
                break;
            }
        }
        if idx < kw_end && sql[idx..kw_end].eq_ignore_ascii_case("LIKE") {
            ValueOperator::Like
        } else {
            return None;
        }
    };

    // Phase 5: Skip whitespace to column identifier.
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }

    // Phase 6: Read column token (optionally qualified with dots/brackets).
    let col_end = idx;
    while idx > 0 {
        let b = bytes[idx - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b']' || b == b'[' {
            idx -= 1;
        } else {
            break;
        }
    }
    let col_token = sql[idx..col_end].trim_matches(|ch| ch == '[' || ch == ']');

    if col_token.is_empty() {
        // Operator found but no column before it — bare context.
        return Some(ValueContext {
            column: None,
            table: None,
            schema: None,
            operator,
            partial_value,
            replace_start: value_start,
            replace_end: cursor,
            has_opening_quote,
        });
    }

    // Parse: schema.table.column, table.column, or bare column.
    let (schema, table, column) = if let Some(dot2) = col_token.rfind('.') {
        let col = col_token[dot2 + 1..].to_string();
        let qualifier = &col_token[..dot2];
        if let Some(dot1) = qualifier.rfind('.') {
            (
                Some(qualifier[..dot1].to_string()),
                Some(qualifier[dot1 + 1..].to_string()),
                col,
            )
        } else {
            (None, Some(qualifier.to_string()), col)
        }
    } else {
        (None, None, col_token.to_string())
    };

    Some(ValueContext {
        column: Some(column),
        table,
        schema,
        operator,
        partial_value,
        replace_start: value_start,
        replace_end: cursor,
        has_opening_quote,
    })
}

/// Resolve the table/schema of a value context against the statement's
/// FROM/JOIN table refs, so a distinct-value query can be built:
///
/// - Bare column (`WHERE Name = `): when the statement has exactly one table
///   ref, attribute the column to it. Ambiguous multi-table statements are
///   left unresolved.
/// - Alias-qualified (`WHERE a.Name = `): replace the alias with the real
///   table name and pick up its schema.
/// - Table-qualified without schema (`WHERE Account.Name = `): fill in the
///   schema from the matching table ref.
fn resolve_value_context_table(mut vc: ValueContext, table_refs: &[TableRef]) -> ValueContext {
    match vc.table.clone() {
        None => {
            if let [only] = table_refs {
                vc.schema = only.schema.clone();
                vc.table = Some(only.name.clone());
            }
        }
        Some(qualifier) if vc.schema.is_none() => {
            if let Some(aliased) = table_refs.iter().find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&qualifier))
            }) {
                vc.schema = aliased.schema.clone();
                vc.table = Some(aliased.name.clone());
            } else if let Some(named) = table_refs
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(&qualifier))
            {
                vc.schema = named.schema.clone();
            }
        }
        Some(_) => {}
    }
    vc
}

/// Consume a quoted value backward from `end`, returning the start position
/// (including the opening quote) and whether an opening quote was found.
fn consume_quoted_value_backward(bytes: &[u8], end: usize) -> (usize, bool) {
    // Count consecutive quotes ending at `end`.
    let mut q_count = 0;
    let mut scan = end;
    while scan > 0 && bytes[scan - 1] == b'\'' {
        q_count += 1;
        scan -= 1;
    }

    if q_count % 2 == 1 {
        // Odd number of quotes — inside an open string.
        // Find the opening quote by scanning backward for the quote that
        // is preceded by a non-quote character (whitespace, =, etc.).
        let mut q_scan = scan;
        // Skip content (non-quote chars) backward.
        while q_scan > 0 && bytes[q_scan - 1] != b'\'' {
            q_scan -= 1;
        }
        if q_scan > 0 && bytes[q_scan - 1] == b'\'' {
            // Found a quote at q_scan - 1. Check if it's the opening quote
            // (preceded by whitespace or = or LIKE).
            let mut before = q_scan - 1;
            while before > 0 && bytes[before - 1].is_ascii_whitespace() {
                before -= 1;
            }
            if before > 0 && (bytes[before - 1] == b'=' || is_like_keyword_end(bytes, before)) {
                return (q_scan - 1, true);
            }
        }
        // Fallback: treat the first quote in the sequence as opening.
        (end - q_count, true)
    } else {
        // Even number of quotes — complete or escaped. No opening quote at cursor.
        (scan, false)
    }
}

/// Check if the bytes before `pos` end with the keyword LIKE.
fn is_like_keyword_end(bytes: &[u8], pos: usize) -> bool {
    let mut idx = pos;
    while idx > 0 {
        let b = bytes[idx - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            idx -= 1;
        } else {
            break;
        }
    }
    let kw = &bytes[idx..pos];
    kw.eq_ignore_ascii_case(b"LIKE")
}

// ---------------------------------------------------------------------------
// Table refs — extract FROM/JOIN table references with aliases
// ---------------------------------------------------------------------------

fn extract_table_refs(statement: &str) -> Vec<TableRef> {
    let words: Vec<&str> = statement
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut refs = Vec::new();
    let mut idx = 0;
    while idx < words.len() {
        let w = words[idx].to_ascii_uppercase();
        if w == "FROM" || w == "JOIN" || w == "INTO" || w == "UPDATE" {
            idx += 1;
            // Skip join modifiers.
            while idx < words.len() {
                let upper = words[idx].to_ascii_uppercase();
                if matches!(
                    upper.as_str(),
                    "INNER" | "LEFT" | "RIGHT" | "OUTER" | "CROSS" | "FULL"
                ) {
                    idx += 1;
                } else {
                    break;
                }
            }
            if idx >= words.len() {
                break;
            }
            let object = words[idx].trim_matches(|ch| ch == '[' || ch == ']');
            let (schema, name) = match object.split_once('.') {
                Some((s, n)) => (Some(s.to_string()), n.to_string()),
                None => (None, object.to_string()),
            };
            let mut alias = None;
            if idx + 1 < words.len() {
                let next = words[idx + 1];
                if next.eq_ignore_ascii_case("AS") && idx + 2 < words.len() {
                    alias = Some(words[idx + 2].to_string());
                    idx += 2;
                } else if !is_clause_keyword(next) {
                    alias = Some(next.to_string());
                    idx += 1;
                }
            }
            refs.push(TableRef {
                schema,
                name,
                alias,
            });
        }
        idx += 1;
    }
    refs
}

fn is_clause_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ON" | "WHERE"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "SET"
            | "VALUES"
            | "AND"
            | "OR"
            | "AS"
            | "CROSS"
            | "FULL"
            | "SELECT"
            | "FROM"
            | "INTO"
            | "UPDATE"
    )
}

// ---------------------------------------------------------------------------
// @-parameter extraction from SQL text
// ---------------------------------------------------------------------------

fn extract_parameters(sql: &str, cursor: usize) -> Vec<String> {
    let before = &sql[..cursor.min(sql.len())];
    let mut params = Vec::new();
    let mut chars = before.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch != '@' || before[idx..].starts_with("@@") {
            continue;
        }
        let start = idx;
        let mut end = idx + 1;
        while let Some((next_idx, next)) = chars.peek().copied() {
            if next.is_ascii_alphanumeric() || next == '_' {
                end = next_idx + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        if end > start + 1 {
            params.push(before[start..end].to_string());
        }
    }
    params
}

// ---------------------------------------------------------------------------
// Auto-trigger gate — decides whether to auto-popup on typing
// ---------------------------------------------------------------------------

/// Whether the completion popup should auto-trigger at the given position.
/// Manual triggers (Ctrl+Space) always bypass this gate.
pub fn should_auto_trigger(sql: &str, cursor: usize, trigger: IntelliSenseTriggerKind) -> bool {
    use IntelliSenseTriggerKind::*;
    if matches!(trigger, ManualShortcut | ManualValueLookup | Explicit) {
        return true;
    }

    let cursor = cursor.min(sql.len());

    // Suppress while typing a number (`= 1▏`, `= 1.▏`): nothing completes a
    // numeric literal, and contains-matching the digits pulled in names like
    // `LOG10` that Enter would then accept (feedback ifb_8765). Checked
    // before the Dot trigger so a decimal point doesn't reopen it.
    if typing_number_literal(sql, cursor) {
        return false;
    }

    if matches!(trigger, Dot | AtSign) {
        return true;
    }

    let before = &sql[..cursor];
    if before.is_empty() {
        return false;
    }

    // Suppress at line start (only whitespace before on this line).
    let line_start = before.rfind('\n').map_or(0, |idx| idx + 1);
    let line_prefix = &before[line_start..];
    if line_prefix.trim().is_empty() {
        return false;
    }

    // Suppress right after `*`.
    if before.trim_end().ends_with('*') {
        return false;
    }

    // Suppress with the caret directly on a comma (`i.Name,|`). The user just
    // closed a list item and is most likely about to press Enter or Space to
    // start the next one — popping up here made Enter accept a suggestion
    // instead of inserting the newline. Deliberately NOT trimmed: once any
    // whitespace follows the comma (`i.Name, |`) the next token has started
    // and the popup is welcome again.
    if before.ends_with(',') {
        return false;
    }

    // Suppress when the current word is a completed SQL keyword.
    let prefix = current_prefix(sql, cursor);
    if !prefix.is_empty() && is_completed_keyword(prefix) {
        return false;
    }

    true
}

/// Whether the word ending at `cursor` is a number being typed: the run of
/// identifier characters and dots before the caret starts with a digit.
/// Identifiers can't start with a digit, so `t1.` or `Col2` don't qualify.
pub fn typing_number_literal(sql: &str, cursor: usize) -> bool {
    let cursor = cursor.min(sql.len());
    let before = &sql.as_bytes()[..cursor];
    let start = before
        .iter()
        .rposition(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$')))
        .map_or(0, |idx| idx + 1);
    before.get(start).is_some_and(u8::is_ascii_digit)
}

fn is_completed_keyword(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "OUTER"
            | "CROSS"
            | "FULL"
            | "ON"
            | "AND"
            | "OR"
            | "NOT"
            | "NULL"
            | "IS"
            | "IN"
            | "LIKE"
            | "BETWEEN"
            | "EXISTS"
            | "GROUP"
            | "ORDER"
            | "BY"
            | "HAVING"
            | "UNION"
            | "ALL"
            | "INSERT"
            | "INTO"
            | "VALUES"
            | "UPDATE"
            | "SET"
            | "DELETE"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "TABLE"
            | "VIEW"
            | "INDEX"
            | "EXEC"
            | "EXECUTE"
            | "DISTINCT"
            | "TOP"
            | "AS"
            | "CASE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "IF"
            | "BEGIN"
            | "RETURN"
            | "WHILE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Multi-byte robustness --------------------------------------------

    #[test]
    fn previous_keyword_after_multibyte_does_not_panic() {
        // `rfind` returns the start byte of the multi-byte separator; the
        // helper must step past its full width rather than land mid-char and
        // panic. Values are quirky (accented chars read as separators) but the
        // contract under test is "never panics".
        for sql in ["Marcó salida", "Marcó", "SET Añoc = ", "a.ñ WHERE "] {
            for (i, _) in sql.char_indices() {
                let _ = previous_keyword(sql, i);
            }
            let _ = previous_keyword(sql, sql.len());
        }
    }

    #[test]
    fn context_auto_sweeps_multibyte_batch_without_panic() {
        let sql = "UPDATE t SET c = 1 -- Marcó\nWHERE id = 1;";
        for (i, _) in sql.char_indices() {
            let _ = CompletionContext::auto(sql, i);
        }
        let _ = CompletionContext::auto(sql, sql.len());
    }

    // ---- Clause detection --------------------------------------------------

    #[test]
    fn clause_select() {
        let ctx = CompletionContext::auto("SELECT c", 8);
        assert_eq!(ctx.clause, IntelliSenseClause::Select);
    }

    #[test]
    fn clause_from() {
        let ctx = CompletionContext::auto("SELECT * FROM ord", 17);
        assert_eq!(ctx.clause, IntelliSenseClause::From);
    }

    #[test]
    fn clause_where() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c", 22);
        assert_eq!(ctx.clause, IntelliSenseClause::Where);
    }

    #[test]
    fn clause_join() {
        let ctx = CompletionContext::auto("SELECT * FROM t JOIN s", 21);
        assert_eq!(ctx.clause, IntelliSenseClause::Join);
    }

    #[test]
    fn clause_on() {
        let ctx = CompletionContext::auto("SELECT * FROM t JOIN s ON t.", 27);
        assert_eq!(ctx.clause, IntelliSenseClause::On);
    }

    #[test]
    fn clause_group_by() {
        let ctx = CompletionContext::auto("SELECT c FROM t GROUP BY c", 25);
        assert_eq!(ctx.clause, IntelliSenseClause::GroupBy);
    }

    #[test]
    fn clause_order_by() {
        let ctx = CompletionContext::auto("SELECT c FROM t ORDER BY c", 25);
        assert_eq!(ctx.clause, IntelliSenseClause::OrderBy);
    }

    #[test]
    fn clause_having() {
        let ctx = CompletionContext::auto("SELECT c FROM t GROUP BY c HAVING c", 33);
        assert_eq!(ctx.clause, IntelliSenseClause::Having);
    }

    #[test]
    fn clause_set() {
        let ctx = CompletionContext::auto("UPDATE t SET c", 13);
        assert_eq!(ctx.clause, IntelliSenseClause::Set);
    }

    #[test]
    fn clause_values() {
        let ctx = CompletionContext::auto("INSERT INTO t (a) VALUES ", 24);
        assert_eq!(ctx.clause, IntelliSenseClause::Values);
    }

    #[test]
    fn clause_exec() {
        let ctx = CompletionContext::auto("EXEC dbo.Get", 12);
        assert_eq!(ctx.clause, IntelliSenseClause::Exec);
    }

    #[test]
    fn clause_unknown_empty() {
        let ctx = CompletionContext::auto("", 0);
        assert_eq!(ctx.clause, IntelliSenseClause::Unknown);
    }

    // ---- Intent detection --------------------------------------------------

    #[test]
    fn intent_select_list() {
        let ctx = CompletionContext::auto("SELECT c", 8);
        assert_eq!(ctx.intent, IntelliSenseIntent::SelectList);
    }

    #[test]
    fn intent_from_source() {
        let ctx = CompletionContext::auto("SELECT * FROM ord", 17);
        assert_eq!(ctx.intent, IntelliSenseIntent::FromSource);
    }

    #[test]
    fn intent_where_predicate() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c", 22);
        assert_eq!(ctx.intent, IntelliSenseIntent::WherePredicate);
    }

    #[test]
    fn intent_join_source() {
        let ctx = CompletionContext::auto("SELECT * FROM t JOIN s", 21);
        assert_eq!(ctx.intent, IntelliSenseIntent::JoinSource);
    }

    #[test]
    fn intent_join_condition_on() {
        let ctx = CompletionContext::auto("SELECT * FROM t JOIN s ON t", 26);
        assert_eq!(ctx.intent, IntelliSenseIntent::JoinCondition);
    }

    #[test]
    fn intent_qualified_member() {
        let ctx = CompletionContext::auto("SELECT t.c FROM t", 10);
        assert_eq!(ctx.intent, IntelliSenseIntent::QualifiedMember);
    }

    #[test]
    fn intent_parameter() {
        let ctx = CompletionContext::auto("SELECT @p", 9);
        assert_eq!(ctx.intent, IntelliSenseIntent::Parameter);
    }

    #[test]
    fn intent_exec_procedure() {
        let ctx = CompletionContext::auto("EXEC dbo.Get", 12);
        assert_eq!(ctx.intent, IntelliSenseIntent::ExecProcedure);
    }

    #[test]
    fn intent_value_literal_after_equals() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = ", 26);
        assert_eq!(ctx.intent, IntelliSenseIntent::ValueLiteral);
    }

    #[test]
    fn intent_value_literal_after_like() {
        // Cursor right after "LIKE " (nothing typed yet).
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c LIKE ", 28);
        assert_eq!(ctx.intent, IntelliSenseIntent::ValueLiteral);
    }

    #[test]
    fn intent_set_assignment() {
        let ctx = CompletionContext::auto("UPDATE t SET c", 13);
        assert_eq!(ctx.intent, IntelliSenseIntent::SetAssignment);
    }

    #[test]
    fn intent_group_by() {
        let ctx = CompletionContext::auto("SELECT c FROM t GROUP BY c", 25);
        assert_eq!(ctx.intent, IntelliSenseIntent::GroupBy);
    }

    #[test]
    fn intent_order_by() {
        let ctx = CompletionContext::auto("SELECT c FROM t ORDER BY c", 25);
        assert_eq!(ctx.intent, IntelliSenseIntent::OrderBy);
    }

    #[test]
    fn intent_having() {
        let ctx = CompletionContext::auto("SELECT c FROM t GROUP BY c HAVING c", 33);
        assert_eq!(ctx.intent, IntelliSenseIntent::Having);
    }

    #[test]
    fn intent_general_unknown_clause() {
        let ctx = CompletionContext::auto("", 0);
        assert_eq!(ctx.intent, IntelliSenseIntent::General);
    }

    // ---- Value context -----------------------------------------------------

    #[test]
    fn value_context_after_equals() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = ", 26);
        assert!(ctx.value_context.is_some());
    }

    #[test]
    fn value_context_after_like() {
        // Cursor right after "LIKE " (nothing typed yet) — should detect.
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c LIKE ", 28);
        assert!(ctx.value_context.is_some());
    }

    #[test]
    fn value_context_suppressed_in_on_clause() {
        let ctx = CompletionContext::auto("SELECT * FROM t JOIN s ON t.a = ", 31);
        assert!(ctx.value_context.is_none());
        // Intent should be join condition, not value literal.
        assert_eq!(ctx.intent, IntelliSenseIntent::JoinCondition);
    }

    #[test]
    fn value_context_not_in_select() {
        let ctx = CompletionContext::auto("SELECT c FROM t", 15);
        assert!(ctx.value_context.is_none());
    }

    #[test]
    fn value_context_equals_resolves_column() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = ", 26);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("c"));
        assert_eq!(vc.operator, ValueOperator::Equals);
        assert!(vc.partial_value.is_empty());
        assert!(!vc.has_opening_quote);
        assert_eq!(vc.replace_start, 26);
        assert_eq!(vc.replace_end, 26);
    }

    #[test]
    fn value_context_like_resolves_column() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c LIKE ", 28);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("c"));
        assert_eq!(vc.operator, ValueOperator::Like);
        assert!(vc.partial_value.is_empty());
    }

    #[test]
    fn value_context_equals_partial_value() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = ab", 28);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.partial_value, "ab");
        assert_eq!(vc.replace_start, 26);
        assert_eq!(vc.replace_end, 28);
        assert!(!vc.has_opening_quote);
    }

    #[test]
    fn value_context_quoted_partial_value() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = 'ab", 29);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.partial_value, "ab");
        assert!(vc.has_opening_quote);
        assert_eq!(vc.replace_start, 26);
        assert_eq!(vc.replace_end, 29);
    }

    #[test]
    fn value_context_opening_quote_only() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = '", 27);
        let vc = ctx.value_context.as_ref().unwrap();
        assert!(vc.partial_value.is_empty());
        assert!(vc.has_opening_quote);
        assert_eq!(vc.replace_start, 26);
    }

    #[test]
    fn value_context_qualified_column() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE u.status = ", 34);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("status"));
        assert_eq!(vc.table.as_deref(), Some("u"));
    }

    #[test]
    fn value_context_schema_qualified_column() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE dbo.users.status = ", 41);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("status"));
        assert_eq!(vc.table.as_deref(), Some("users"));
        assert_eq!(vc.schema.as_deref(), Some("dbo"));
    }

    #[test]
    fn value_context_bare_column_resolves_single_table_ref() {
        let sql = "SELECT\n    *\nFROM\n    Accounting.Account\nWHERE\n    Name = ";
        let ctx = CompletionContext::auto(sql, sql.len());
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("Name"));
        assert_eq!(vc.table.as_deref(), Some("Account"));
        assert_eq!(vc.schema.as_deref(), Some("Accounting"));
        assert_eq!(vc.operator, ValueOperator::Equals);
    }

    #[test]
    fn value_context_alias_resolves_to_table_ref() {
        let sql = "SELECT * FROM Accounting.Account a WHERE a.Name = ";
        let ctx = CompletionContext::auto(sql, sql.len());
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("Name"));
        assert_eq!(vc.table.as_deref(), Some("Account"));
        assert_eq!(vc.schema.as_deref(), Some("Accounting"));
    }

    #[test]
    fn value_context_table_qualifier_fills_schema() {
        let sql = "SELECT * FROM Accounting.Account WHERE Account.Name = ";
        let ctx = CompletionContext::auto(sql, sql.len());
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.table.as_deref(), Some("Account"));
        assert_eq!(vc.schema.as_deref(), Some("Accounting"));
    }

    #[test]
    fn value_context_bare_column_ambiguous_tables_unresolved() {
        let sql = "SELECT * FROM A JOIN B ON A.x = B.y WHERE Name = ";
        let ctx = CompletionContext::auto(sql, sql.len());
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.column.as_deref(), Some("Name"));
        assert!(vc.table.is_none());
    }

    #[test]
    fn value_context_bare_column_with_partial_resolves_table() {
        let sql = "SELECT * FROM Accounting.Account WHERE Name = Che";
        let ctx = CompletionContext::auto(sql, sql.len());
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.table.as_deref(), Some("Account"));
        assert_eq!(vc.partial_value, "Che");
    }

    #[test]
    fn value_context_rejects_compound_operators() {
        assert!(
            CompletionContext::auto("SELECT * FROM t WHERE c <= ", 26)
                .value_context
                .is_none()
        );
        assert!(
            CompletionContext::auto("SELECT * FROM t WHERE c >= ", 26)
                .value_context
                .is_none()
        );
        assert!(
            CompletionContext::auto("SELECT * FROM t WHERE c != ", 26)
                .value_context
                .is_none()
        );
        assert!(
            CompletionContext::auto("SELECT * FROM t WHERE c <> ", 26)
                .value_context
                .is_none()
        );
    }

    #[test]
    fn value_context_like_with_partial_value() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c LIKE ab", 31);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.operator, ValueOperator::Like);
        assert_eq!(vc.partial_value, "ab");
        assert_eq!(vc.column.as_deref(), Some("c"));
    }

    #[test]
    fn value_context_like_with_quoted_partial() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c LIKE 'ab", 32);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.operator, ValueOperator::Like);
        assert_eq!(vc.partial_value, "ab");
        assert!(vc.has_opening_quote);
    }

    #[test]
    fn value_context_numeric_partial_value() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = 42", 28);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.partial_value, "42");
    }

    #[test]
    fn value_context_negative_numeric_partial() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = -5", 28);
        let vc = ctx.value_context.as_ref().unwrap();
        assert_eq!(vc.partial_value, "-5");
    }

    // ---- Qualifier & replacement span --------------------------------------

    #[test]
    fn qualifier_dotted_prefix() {
        let ctx = CompletionContext::auto("SELECT sales.ord", 16);
        assert!(ctx.is_qualified_access);
        assert_eq!(ctx.qualifier.as_deref(), Some("sales"));
        assert_eq!(ctx.replace_text, "ord");
    }

    #[test]
    fn replace_span_simple() {
        let ctx = CompletionContext::auto("SELECT or", 9);
        assert_eq!(ctx.replace_start, 7);
        assert_eq!(ctx.replace_end, 9);
    }

    #[test]
    fn replace_span_dotted() {
        let ctx = CompletionContext::auto("SELECT sales.ord", 16);
        // Only "ord" (3 chars) should be replaced, not "sales.ord" (9 chars).
        assert_eq!(ctx.replace_start, 13);
        assert_eq!(ctx.replace_end, 16);
    }

    // ---- Table refs --------------------------------------------------------

    #[test]
    fn table_refs_single() {
        let ctx = CompletionContext::auto("SELECT * FROM Orders", 20);
        assert_eq!(ctx.table_refs.len(), 1);
        assert_eq!(ctx.table_refs[0].name, "Orders");
        assert!(ctx.table_refs[0].alias.is_none());
    }

    #[test]
    fn table_refs_with_alias() {
        let ctx = CompletionContext::auto("SELECT * FROM Orders o", 22);
        assert_eq!(ctx.table_refs.len(), 1);
        assert_eq!(ctx.table_refs[0].alias.as_deref(), Some("o"));
    }

    #[test]
    fn table_refs_with_schema() {
        let ctx = CompletionContext::auto("SELECT * FROM dbo.Orders", 24);
        assert_eq!(ctx.table_refs.len(), 1);
        assert_eq!(ctx.table_refs[0].schema.as_deref(), Some("dbo"));
        assert_eq!(ctx.table_refs[0].name, "Orders");
    }

    #[test]
    fn table_refs_join() {
        let ctx = CompletionContext::auto("SELECT * FROM Orders o JOIN Items i", 35);
        assert_eq!(ctx.table_refs.len(), 2);
        assert_eq!(ctx.table_refs[0].name, "Orders");
        assert_eq!(ctx.table_refs[1].name, "Items");
    }

    // ---- Aliases -----------------------------------------------------------

    #[test]
    fn aliases_detected() {
        let ctx = CompletionContext::auto("SELECT * FROM Orders o JOIN Items i", 35);
        assert!(ctx.has_aliases);
        assert_eq!(ctx.alias_count, 2);
    }

    #[test]
    fn no_aliases_simple() {
        let ctx = CompletionContext::auto("SELECT * FROM Orders", 20);
        assert!(!ctx.has_aliases);
    }

    // ---- @-parameters ------------------------------------------------------

    #[test]
    fn parameters_extracted() {
        let ctx = CompletionContext::auto("DECLARE @id int; SELECT @id", 27);
        assert!(ctx.parameters.contains(&"@id".to_string()));
    }

    // ---- Auto-trigger gate -------------------------------------------------

    #[test]
    fn auto_trigger_line_start_false() {
        assert!(!should_auto_trigger(
            "   ",
            3,
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    #[test]
    fn auto_trigger_after_star_false() {
        assert!(!should_auto_trigger(
            "SELECT * ",
            9,
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    #[test]
    fn auto_trigger_completed_keyword_false() {
        assert!(!should_auto_trigger(
            "SELECT FROM ",
            11,
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    // A trailing comma is where the user ends a select-list line before
    // pressing Enter (`i.Name,⏎`). The popup must not auto-open there:
    // with it open, Enter/Tab were captured by the suggestion list and the
    // expected newline never happened.
    #[test]
    fn auto_trigger_directly_after_comma_false() {
        let sql = "SELECT\n\ti.InternalItemCode,\n\ti.Name,";
        assert!(!should_auto_trigger(
            sql,
            sql.len(),
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    // …but a space after the comma means the next list item has started, so
    // normal automatic completion resumes.
    #[test]
    fn auto_trigger_after_comma_and_space_true() {
        let sql = "SELECT i.InternalItemCode, ";
        assert!(should_auto_trigger(
            sql,
            sql.len(),
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    // Ctrl+Space is an explicit request — it bypasses the comma gate like
    // every other automatic-only suppression.
    #[test]
    fn manual_trigger_directly_after_comma_true() {
        let sql = "SELECT i.Name,";
        assert!(should_auto_trigger(
            sql,
            sql.len(),
            IntelliSenseTriggerKind::ManualShortcut
        ));
    }

    #[test]
    fn auto_trigger_mid_word_true() {
        assert!(should_auto_trigger(
            "SELECT * FROM Ord",
            18,
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    #[test]
    fn auto_trigger_dot_true() {
        assert!(should_auto_trigger(
            "SELECT t.",
            9,
            IntelliSenseTriggerKind::Dot
        ));
    }

    #[test]
    fn auto_trigger_at_sign_true() {
        assert!(should_auto_trigger(
            "SELECT @",
            8,
            IntelliSenseTriggerKind::AtSign
        ));
    }

    #[test]
    fn auto_trigger_manual_always_true() {
        assert!(should_auto_trigger(
            "   ",
            3,
            IntelliSenseTriggerKind::ManualShortcut
        ));
    }

    #[test]
    fn auto_trigger_empty_false() {
        assert!(!should_auto_trigger(
            "",
            0,
            IntelliSenseTriggerKind::AutomaticTyping
        ));
    }

    // ---- Statement isolation -----------------------------------------------

    #[test]
    fn statement_isolation_semicolon() {
        let ctx = CompletionContext::auto("SELECT 1; SELECT c", 17);
        assert_eq!(ctx.clause, IntelliSenseClause::Select);
    }

    /// Feedback ifb_3798: `update ` after a finished, aliased SELECT is a new
    /// statement — nothing of the SELECT (clause, sources, alias) reaches it.
    #[test]
    fn statement_isolation_unterminated_statement_keyword() {
        let sql = "SELECT\n\t*\nFROM\n\tAccounting.ReportingPeriod AS arp\n\t\nupdate ";
        let ctx = CompletionContext::auto(sql, sql.len());
        assert_eq!(ctx.statement_lower, "update ");
        assert_eq!(ctx.clause, IntelliSenseClause::Update);
        assert_eq!(ctx.intent, IntelliSenseIntent::FromSource);
        assert!(ctx.table_refs.is_empty(), "{:?}", ctx.table_refs);
        assert!(ctx.aliases.is_empty(), "{:?}", ctx.aliases);
    }

    #[test]
    fn statement_head_keywords_with_an_empty_prefix_are_object_positions() {
        for sql in [
            "UPDATE ",
            "DELETE ",
            "INSERT ",
            "MERGE ",
            "SELECT 1\nGO\nUPDATE ",
            "UPDATE t SET x = 1;\nDELETE ",
            "SELECT * FROM a\n\ninsert ",
        ] {
            let ctx = CompletionContext::auto(sql, sql.len());
            assert_eq!(ctx.intent, IntelliSenseIntent::FromSource, "{sql:?}");
        }
    }

    #[test]
    fn previous_keyword_is_the_last_word_once_a_separator_follows_it() {
        assert_eq!(previous_keyword("UPDATE ", 7), "UPDATE");
        assert_eq!(previous_keyword("UPDATE Ord", 10), "UPDATE");
        assert_eq!(previous_keyword("SELECT * FROM t WHERE ", 22), "WHERE");
        assert_eq!(previous_keyword("SELECT * FROM t WHERE c", 23), "WHERE");
    }
}
