//! Clause-context detection and query-scope table-reference extraction.
//!
//! Determines which SQL clause the cursor is inside (driving which completion
//! kinds are offered) and extracts the FROM/JOIN table references and CTE
//! names in the cursor's lexical scope, using the depth-filtered token stream
//! from [`super::scan`].

use sqlly_lang::sqlmodel::Dialect;

use super::scan;

/// The SQL clause the cursor is currently inside, determined by a backward
/// scan for the most recent clause keyword. Drives which completion kinds are
/// offered so the editor suggests columns in WHERE / GROUP BY / ORDER BY /
/// JOIN ON (not just after SELECT) and tables primarily in FROM / JOIN
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClauseContext {
    /// Before any clause keyword, or inside a construct we don't model.
    Unknown,
    /// Between SELECT and FROM — column expressions, functions, aliases.
    SelectList,
    /// Between FROM and the next clause keyword — table/view sources.
    FromClause,
    /// Right after a JOIN keyword — table/view source for the joined table.
    JoinTarget,
    /// Between ON and the next clause keyword — join condition expressions.
    JoinOn,
    /// Between WHERE and GROUP BY / HAVING / ORDER BY / end — filter
    /// expressions.
    WhereClause,
    /// Between GROUP BY and HAVING / ORDER BY / end — grouping expressions.
    GroupByClause,
    /// Between HAVING and ORDER BY / end — aggregate filter expressions.
    HavingClause,
    /// Between ORDER BY and end / next statement — sort expressions.
    OrderByClause,
    /// Between SET and WHERE / end in UPDATE — column assignments.
    SetClause,
}

impl ClauseContext {
    /// `true` when the cursor is in a position that expects column references
    /// (SELECT list, WHERE, GROUP BY, HAVING, ORDER BY, JOIN ON, SET).
    pub(crate) fn is_column_expecting(self) -> bool {
        matches!(
            self,
            ClauseContext::SelectList
                | ClauseContext::WhereClause
                | ClauseContext::GroupByClause
                | ClauseContext::HavingClause
                | ClauseContext::OrderByClause
                | ClauseContext::JoinOn
                | ClauseContext::SetClause
        )
    }

    /// `true` when the cursor is in a position that expects a table/view
    /// reference (FROM clause, JOIN target).
    pub(crate) fn is_table_expecting(self) -> bool {
        matches!(self, ClauseContext::FromClause | ClauseContext::JoinTarget)
    }
}

/// A table reference extracted from a FROM/JOIN clause before the cursor, with
/// its optional alias. Used to scope column completions to tables that are
/// actually in the query.
#[derive(Debug, Clone)]
pub(crate) struct TableRef {
    pub(crate) name: String,
    pub(crate) alias: Option<String>,
    /// A derived table's (`FROM (SELECT …) t`) output columns, read from its
    /// select list; `None` for a named source, whose columns come from the
    /// catalog. See feedback ifb_8e1f.
    pub(crate) columns: Option<Vec<String>>,
}

/// Detects which SQL clause the cursor is currently inside by scanning
/// backward from the cursor for the most recent clause keyword. Handles
/// multi-word keywords (GROUP BY, ORDER BY) and JOIN variants. The scan is
/// O(n) in the text before the cursor — well within the intellisense latency
/// budget.
pub(crate) fn detect_clause_context(sql: &str, cursor: usize) -> ClauseContext {
    let cursor = cursor.min(sql.len());
    let words = depth_words(sql, cursor, true);

    if words.is_empty() {
        return ClauseContext::Unknown;
    }

    let mut i = words.len();
    while i > 0 {
        i -= 1;
        let word = &words[i];

        // Multi-word: GROUP BY / ORDER BY — the "BY" token is the anchor.
        if word.eq_ignore_ascii_case("BY") && i > 0 {
            let prev = &words[i - 1];
            if prev.eq_ignore_ascii_case("GROUP") {
                return ClauseContext::GroupByClause;
            }
            if prev.eq_ignore_ascii_case("ORDER") {
                return ClauseContext::OrderByClause;
            }
        }

        if word.eq_ignore_ascii_case("SELECT") {
            return ClauseContext::SelectList;
        }
        if word.eq_ignore_ascii_case("FROM") {
            return ClauseContext::FromClause;
        }
        if word.eq_ignore_ascii_case("WHERE") {
            return ClauseContext::WhereClause;
        }
        if word.eq_ignore_ascii_case("HAVING") {
            return ClauseContext::HavingClause;
        }
        if word.eq_ignore_ascii_case("ON") {
            return ClauseContext::JoinOn;
        }
        if word.eq_ignore_ascii_case("JOIN") {
            if join_condition_without_on(sql, cursor) {
                return ClauseContext::JoinOn;
            }
            return ClauseContext::JoinTarget;
        }
        if word.eq_ignore_ascii_case("SET") {
            let mut j = i;
            let mut found_update = false;
            while j > 0 {
                j -= 1;
                let prev = &words[j];
                if prev.eq_ignore_ascii_case("UPDATE") {
                    found_update = true;
                    break;
                }
                if prev.eq_ignore_ascii_case("SELECT")
                    || prev.eq_ignore_ascii_case("INSERT")
                    || prev.eq_ignore_ascii_case("DELETE")
                    || prev.eq_ignore_ascii_case("MERGE")
                    || prev.eq_ignore_ascii_case("FROM")
                    || prev.eq_ignore_ascii_case("WHERE")
                    || prev.eq_ignore_ascii_case("GROUP")
                    || prev.eq_ignore_ascii_case("ORDER")
                    || prev.eq_ignore_ascii_case("HAVING")
                    || prev.eq_ignore_ascii_case("JOIN")
                    || prev.eq_ignore_ascii_case("BY")
                {
                    break;
                }
            }
            if found_update {
                return ClauseContext::SetClause;
            }
        }
    }

    ClauseContext::Unknown
}

/// Extracts table references (with optional aliases) from all FROM/JOIN
/// clauses in the cursor's lexical scope. Scans the **full** SQL text (not
/// just up to the cursor) so that table definitions after the cursor are
/// visible — this enables fill-in-the-middle completion (`SELECT cu FROM
/// Orders`). Parenthesis-depth filtering ensures only tables at the
/// cursor's nesting level are collected, correctly handling subqueries and
/// CTEs.
pub(crate) fn query_table_refs(sql: &str, cursor: usize) -> Vec<TableRef> {
    let (tokens, depth) = statement_tokens(sql, cursor);
    let mut refs = query_table_refs_at_depth(&tokens, depth);
    fill_script_table_columns(sql, &mut refs);
    refs
}

/// Give each source the script itself creates — `SELECT … INTO #t`,
/// `CREATE TABLE #t (…)` — the columns that definition yields, so `t.▏`
/// on a temp table completes like a catalog table (feedback ifb_0b20). Only
/// sources with no columns yet are touched, and only when the script defines
/// a table of that name.
pub(crate) fn fill_script_table_columns(sql: &str, refs: &mut [TableRef]) {
    if !refs.iter().any(|r| r.columns.is_none()) {
        return;
    }
    let tables = script_tables(sql);
    if tables.is_empty() {
        return;
    }
    for table_ref in refs.iter_mut().filter(|r| r.columns.is_none()) {
        if let Some(table) = tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(&table_ref.name))
            && let Some(columns) = &table.columns
        {
            table_ref.columns = Some(columns.clone());
        }
    }
}

/// A table the script itself creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptTable {
    /// The name as written (`#temp`, `Staging`, `dbo.Loaded`).
    pub(crate) name: String,
    /// The columns the definition yields: a `SELECT … INTO` select list, a
    /// `CREATE TABLE (…)` column list, or a `CREATE TABLE … AS SELECT` select
    /// list. `None` for an `INSERT INTO #t` with no definition in the script.
    pub(crate) columns: Option<Vec<String>>,
    /// Document-absolute byte offset of the defining statement's keyword.
    pub(crate) defined_at: usize,
    /// A temporary table: a `#`-prefixed name, or one created with
    /// `TEMP` / `TEMPORARY`.
    pub(crate) is_temp: bool,
}

/// Every table the script creates, in document order: `SELECT … INTO [TEMP]
/// name`, `CREATE [TEMP|TEMPORARY] TABLE [IF NOT EXISTS] name (…)` or
/// `… AS SELECT`, and `INSERT INTO #name` (SQL Server temp names only, with
/// no columns). The whole buffer is scanned, not just the caret's
/// statement: a temp table is defined once and read by the statements
/// after it (feedback ifb_0b20, ifb_416b).
pub(crate) fn script_tables(sql: &str) -> Vec<ScriptTable> {
    use scan::ScanTokenKind::{Comma, LeftParen, RightParen, Word};
    let tokens = scan::scan_tokens(sql);
    let is = |t: &scan::ScanToken<'_>, w: &str| {
        t.kind == Word && t.text.as_str().eq_ignore_ascii_case(w)
    };
    let mut out: Vec<ScriptTable> = Vec::new();
    let push = |table: ScriptTable, out: &mut Vec<ScriptTable>| match out
        .iter_mut()
        .find(|t| t.name.eq_ignore_ascii_case(&table.name))
    {
        Some(existing) => {
            if existing.columns.is_none() {
                existing.columns = table.columns;
            }
        }
        None => out.push(table),
    };
    // Index of the last statement-head keyword seen at each depth, so an
    // `INTO` can be told apart (`SELECT … INTO` vs `INSERT INTO`).
    let mut last_head: Vec<Option<(usize, String)>> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let depth = t.paren_depth;
        if last_head.len() <= depth {
            last_head.resize(depth + 1, None);
        }
        if t.kind != Word {
            i += 1;
            continue;
        }
        let upper = t.text.as_str().to_ascii_uppercase();
        match upper.as_str() {
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "CREATE" | "DROP" | "WITH" => {
                last_head[depth] = Some((i, upper.clone()));
            }
            _ => {}
        }
        match upper.as_str() {
            "INTO" => {
                let Some((head_idx, head)) = last_head[depth].clone() else {
                    i += 1;
                    continue;
                };
                // `SELECT … INTO [TEMP|TEMPORARY] [TABLE] name`.
                if head == "SELECT" {
                    let mut j = i + 1;
                    let mut is_temp = false;
                    while j < tokens.len()
                        && (is(&tokens[j], "TEMP")
                            || is(&tokens[j], "TEMPORARY")
                            || is(&tokens[j], "UNLOGGED")
                            || is(&tokens[j], "TABLE"))
                    {
                        is_temp |= is(&tokens[j], "TEMP") || is(&tokens[j], "TEMPORARY");
                        j += 1;
                    }
                    if let Some((name, next)) = dotted_name_at(&tokens, j) {
                        let columns = derived_columns(&tokens[head_idx..i], depth);
                        push(
                            ScriptTable {
                                is_temp: is_temp || name.starts_with('#'),
                                name,
                                columns: Some(columns),
                                defined_at: tokens[head_idx].byte_offset,
                            },
                            &mut out,
                        );
                        i = next;
                        continue;
                    }
                } else if head == "INSERT"
                    && let Some((name, next)) = dotted_name_at(&tokens, i + 1)
                    && name.starts_with('#')
                {
                    push(
                        ScriptTable {
                            name,
                            columns: None,
                            defined_at: tokens[head_idx].byte_offset,
                            is_temp: true,
                        },
                        &mut out,
                    );
                    i = next;
                    continue;
                }
            }
            "CREATE" => {
                let mut j = i + 1;
                let mut is_temp = false;
                while j < tokens.len()
                    && tokens[j].kind == Word
                    && !is(&tokens[j], "TABLE")
                    && [
                        "GLOBAL",
                        "LOCAL",
                        "TEMP",
                        "TEMPORARY",
                        "UNLOGGED",
                        "OR",
                        "REPLACE",
                    ]
                    .iter()
                    .any(|w| is(&tokens[j], w))
                {
                    is_temp |= is(&tokens[j], "TEMP") || is(&tokens[j], "TEMPORARY");
                    j += 1;
                }
                if j >= tokens.len() || !is(&tokens[j], "TABLE") {
                    i += 1;
                    continue;
                }
                j += 1;
                if j + 2 < tokens.len()
                    && is(&tokens[j], "IF")
                    && is(&tokens[j + 1], "NOT")
                    && is(&tokens[j + 2], "EXISTS")
                {
                    j += 3;
                }
                let Some((name, next)) = dotted_name_at(&tokens, j) else {
                    i += 1;
                    continue;
                };
                let columns = if next < tokens.len() && tokens[next].kind == LeftParen {
                    // `(col type …, CONSTRAINT …)`: the first word of each
                    // top-level item that is not a constraint clause.
                    let inner = depth + 1;
                    let close = tokens[next + 1..]
                        .iter()
                        .position(|t| t.paren_depth == depth && t.kind == RightParen)
                        .map_or(tokens.len(), |o| next + 1 + o);
                    let mut columns = Vec::new();
                    let mut item_start = true;
                    for t in &tokens[next + 1..close] {
                        if t.paren_depth == inner && t.kind == Comma {
                            item_start = true;
                            continue;
                        }
                        if item_start && t.paren_depth == inner {
                            item_start = false;
                            if t.kind == Word
                                && ![
                                    "CONSTRAINT",
                                    "PRIMARY",
                                    "FOREIGN",
                                    "UNIQUE",
                                    "CHECK",
                                    "INDEX",
                                    "KEY",
                                    "PERIOD",
                                    "LIKE",
                                    "FULLTEXT",
                                    "SPATIAL",
                                    "EXCLUDE",
                                ]
                                .iter()
                                .any(|w| is(t, w))
                            {
                                columns.push(t.text.as_str().to_string());
                            }
                        }
                    }
                    Some(columns)
                } else if next < tokens.len() && is(&tokens[next], "AS") {
                    // `CREATE TABLE name AS SELECT …`: the select list.
                    let end = tokens[next + 1..]
                        .iter()
                        .position(|t| {
                            t.paren_depth < depth
                                || (t.paren_depth == depth
                                    && t.kind == scan::ScanTokenKind::Semicolon)
                        })
                        .map_or(tokens.len(), |o| next + 1 + o);
                    Some(derived_columns(&tokens[next + 1..end], depth))
                } else {
                    Some(Vec::new())
                };
                push(
                    ScriptTable {
                        is_temp: is_temp || name.starts_with('#'),
                        name,
                        columns,
                        defined_at: t.byte_offset,
                    },
                    &mut out,
                );
                i = next;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Every `DROP [TEMPORARY] TABLE [IF EXISTS] name[, name…]` in the script:
/// each dropped name with the byte offset of its `DROP`.
pub(crate) fn script_drops(sql: &str) -> Vec<(String, usize)> {
    use scan::ScanTokenKind::{Comma, Word};
    let tokens = scan::scan_tokens(sql);
    let is = |t: &scan::ScanToken<'_>, w: &str| {
        t.kind == Word && t.text.as_str().eq_ignore_ascii_case(w)
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !is(&tokens[i], "DROP") {
            i += 1;
            continue;
        }
        let at = tokens[i].byte_offset;
        let mut j = i + 1;
        while j < tokens.len() && (is(&tokens[j], "TEMPORARY") || is(&tokens[j], "TEMP")) {
            j += 1;
        }
        if j >= tokens.len() || !is(&tokens[j], "TABLE") {
            i += 1;
            continue;
        }
        j += 1;
        if j + 1 < tokens.len() && is(&tokens[j], "IF") && is(&tokens[j + 1], "EXISTS") {
            j += 2;
        }
        while let Some((name, next)) = dotted_name_at(&tokens, j) {
            out.push((name, at));
            if next < tokens.len() && tokens[next].kind == Comma {
                j = next + 1;
            } else {
                break;
            }
        }
        i = j.max(i + 1);
    }
    out
}

/// The `DROP [TEMPORARY] TABLE [IF EXISTS] ▏` head of the caret's statement:
/// `true` when the cursor sits where a table name is expected after it.
pub fn at_drop_table_name(sql: &str, cursor: usize) -> bool {
    let words = depth_words(sql, cursor, true);
    let mut words = words.iter().map(|w| w.to_ascii_uppercase());
    if words.next().as_deref() != Some("DROP") {
        return false;
    }
    let mut next = words.next();
    if matches!(next.as_deref(), Some("TEMPORARY" | "TEMP")) {
        next = words.next();
    }
    if next.as_deref() != Some("TABLE") {
        return false;
    }
    let rest: Vec<String> = words.collect();
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let rest = match rest.as_slice() {
        ["IF", "EXISTS", tail @ ..] => tail,
        tail => tail,
    };
    // With the caret on a word (`DROP TABLE #te▏`) that word is the name
    // being typed; past it nothing else may follow before the name.
    let mid_word = sql[..cursor.min(sql.len())]
        .chars()
        .last()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '#');
    match rest {
        [] => true,
        [_] => mid_word,
        _ => false,
    }
}

/// The dotted identifier starting at token `i` (`a`, `a.b`, `a.b.c`) and the
/// index of the first token after it.
fn dotted_name_at(tokens: &[scan::ScanToken<'_>], i: usize) -> Option<(String, usize)> {
    use scan::ScanTokenKind::{Dot, Word};
    let first = tokens.get(i)?;
    if first.kind != Word || looks_like_keyword(first.text.as_str()) {
        return None;
    }
    let mut name = first.text.as_str().to_string();
    let mut j = i + 1;
    while j + 1 < tokens.len()
        && tokens[j].kind == Dot
        && tokens[j + 1].kind == Word
        && tokens[j].paren_depth == first.paren_depth
    {
        name.push('.');
        name.push_str(tokens[j + 1].text.as_str());
        j += 2;
    }
    Some((name, j))
}

/// Whether the caret's JOIN clause already carries its condition without an
/// `ON`: `JOIN T t t.Col = ▏`. The comparison makes the position a join
/// condition, not a join source or a literal value (feedback ifb_e85a).
pub fn join_condition_without_on(sql: &str, cursor: usize) -> bool {
    let cursor = cursor.min(sql.len());
    let (tokens, depth) = statement_tokens(sql, cursor);
    let before: Vec<&scan::ScanToken<'_>> = tokens
        .iter()
        .filter(|t| t.paren_depth == depth && t.byte_offset < cursor)
        .collect();
    let Some(join) = before.iter().rposition(|t| {
        t.kind == scan::ScanTokenKind::Word && t.text.as_str().eq_ignore_ascii_case("JOIN")
    }) else {
        return false;
    };
    let after = &before[join + 1..];
    if after.iter().any(|t| {
        t.kind == scan::ScanTokenKind::Word
            && (t.text.as_str().eq_ignore_ascii_case("ON") || is_reserved_alias(t.text.as_str()))
    }) {
        return false;
    }
    after
        .iter()
        .any(|t| t.kind == scan::ScanTokenKind::Other && t.text.as_str().contains('='))
}

/// Whether the caret sits right after a complete JOIN source and its alias,
/// with no `ON` written yet: `JOIN Accounting.FinancialAccountingKey AS fak ▏`.
/// The next thing that belongs there is the join's condition, not another
/// table (feedback ifb_e275). A word being typed at the caret counts as the
/// start of that condition — unless it is the alias itself (`JOIN T fa▏`).
///
/// A `CROSS JOIN` or `NATURAL [LEFT|RIGHT|FULL|INNER] [OUTER] JOIN` takes no
/// condition, so after its source and alias the clause is complete and the
/// usual keywords (`WHERE`, another join, …) are what belong next.
pub fn join_awaiting_on(sql: &str, cursor: usize) -> bool {
    use scan::ScanTokenKind::Word;
    let cursor = cursor.min(sql.len());
    let (tokens, depth) = statement_tokens(sql, cursor);
    let mut after: Vec<scan::ScanToken<'_>> = Vec::new();
    let mut conditionless = false;
    // The words leading up to the current JOIN, most recent last.
    let mut lead: Vec<String> = Vec::new();
    for token in tokens {
        if token.paren_depth != depth || token.byte_offset >= cursor {
            continue;
        }
        let word = (token.kind == Word).then(|| token.text.as_str().to_ascii_uppercase());
        if word.as_deref() == Some("JOIN") {
            conditionless = join_takes_no_condition(&lead);
            after.clear();
            after.push(token);
        } else if !after.is_empty() {
            after.push(token);
        }
        match word {
            Some(word) => {
                if lead.len() == 3 {
                    lead.remove(0);
                }
                lead.push(word);
            }
            None => lead.clear(),
        }
    }
    if after.is_empty() || conditionless {
        return false;
    }
    let mid_word = sql[..cursor]
        .chars()
        .last()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '#' | '$'));
    let end = if mid_word {
        after.len() - 1
    } else {
        after.len()
    };
    let tail = &after[1..end.max(1)];
    let Some((_, mut i)) = dotted_name_at(tail, 0) else {
        return false;
    };
    if tail
        .get(i)
        .is_some_and(|t| t.kind == Word && t.text.as_str().eq_ignore_ascii_case("AS"))
    {
        i += 1;
    }
    let Some(alias) = tail.get(i).filter(|t| t.kind == Word) else {
        return false;
    };
    let alias = alias.text.as_str();
    !looks_like_keyword(alias) && !is_reserved_alias(alias) && i + 1 == tail.len()
}

/// Whether the join introduced by the words in `lead` (those right before
/// `JOIN`, most recent last) takes no `ON` condition: `CROSS JOIN`, or
/// `NATURAL` with any join type and an optional `OUTER` between.
fn join_takes_no_condition(lead: &[String]) -> bool {
    let mut words = lead.iter().rev().map(String::as_str);
    let Some(last) = words.next() else {
        return false;
    };
    if last.eq_ignore_ascii_case("CROSS") || last.eq_ignore_ascii_case("NATURAL") {
        return true;
    }
    let join_type = if last.eq_ignore_ascii_case("OUTER") {
        words.next()
    } else {
        Some(last)
    };
    let is_join_type = join_type.is_some_and(|w| {
        ["LEFT", "RIGHT", "FULL", "INNER"]
            .iter()
            .any(|t| w.eq_ignore_ascii_case(t))
    });
    is_join_type
        && words
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("NATURAL"))
}

/// The scanned tokens of the statement that contains `cursor` — offsets
/// document-absolute — and the cursor's parenthesis depth within it. Every
/// clause, alias and table-reference decision reads this stream rather than
/// the whole buffer, so a previous statement's FROM sources, aliases and
/// unbalanced parentheses never leak into the caret's statement (feedback
/// ifb_3798). See [`crate::intellisense::statement_range`].
pub(crate) fn statement_tokens(sql: &str, cursor: usize) -> (Vec<scan::ScanToken<'_>>, usize) {
    let range = crate::intellisense::statement_range(sql, cursor, Dialect::SqlServer);
    let mut tokens = scan::scan_tokens(&sql[range.clone()]);
    for token in &mut tokens {
        token.byte_offset += range.start;
        token.end_byte += range.start;
    }
    let depth = scan::cursor_depth(&tokens, cursor);
    (tokens, depth)
}

/// Like [`query_table_refs`] but for an explicit parenthesis depth over an
/// already-scanned token stream. Used to walk outward from the cursor's scope
/// through its enclosing scopes (one scan, many depths) so alias member-access
/// can see aliases declared in an outer query.
pub(crate) fn query_table_refs_at_depth(
    tokens: &[scan::ScanToken<'_>],
    depth: usize,
) -> Vec<TableRef> {
    let merged = depth_merged_tokens_at(tokens, depth);

    let mut refs = Vec::new();
    refs.extend(detect_cte_names(&merged));
    let cte_count = refs.len();

    let mut idx = 0;
    while idx < merged.len() {
        let word = merged[idx].text.as_str();
        if word.eq_ignore_ascii_case("FROM") || word.eq_ignore_ascii_case("JOIN") {
            idx += 1;
            loop {
                while idx < merged.len() && merged[idx].is_comma {
                    idx += 1;
                }
                if idx >= merged.len() {
                    break;
                }

                let object_idx = idx;
                let object = &merged[idx].text;
                if object.is_empty() || is_reserved_alias(object) {
                    break;
                }

                let mut alias_idx = idx + 1;
                if alias_idx < merged.len()
                    && !merged[alias_idx].is_comma
                    && merged[alias_idx].text.eq_ignore_ascii_case("AS")
                {
                    alias_idx += 1;
                }

                let alias;
                if alias_idx < merged.len() && !merged[alias_idx].is_comma {
                    let alias_word = &merged[alias_idx].text;
                    if alias_word.is_empty() || is_reserved_alias(alias_word) {
                        alias = None;
                    } else {
                        alias = Some(alias_word.clone());
                        idx = alias_idx;
                    }
                } else {
                    alias = None;
                }

                refs.push(TableRef {
                    name: object.clone(),
                    alias,
                    columns: merged[object_idx].derived.clone(),
                });

                if idx + 1 < merged.len() && merged[idx + 1].is_comma {
                    idx += 1;
                } else {
                    break;
                }
            }
        }
        idx += 1;
    }
    fill_cte_columns(tokens, depth, &merged, &mut refs[cte_count..]);
    refs
}

/// Give each named source that is a CTE (`FROM cte_name [AS] alias`) the
/// CTE's output columns. The CTE may be declared at this depth or in any
/// enclosing scope — a CTE body reading an earlier CTE sits one level in.
fn fill_cte_columns(
    tokens: &[scan::ScanToken<'_>],
    depth: usize,
    merged: &[MergedTok],
    refs: &mut [TableRef],
) {
    if !refs
        .iter()
        .any(|r| r.columns.is_none() && !r.name.contains('.'))
    {
        return;
    }
    let mut ctes = detect_ctes(merged);
    for level in (0..depth).rev() {
        ctes.extend(detect_ctes(&depth_merged_tokens_at(tokens, level)));
    }
    for table_ref in refs.iter_mut().filter(|r| r.columns.is_none()) {
        if let Some(cte) = ctes
            .iter()
            .find(|cte| cte.name.eq_ignore_ascii_case(&table_ref.name))
            && let Some(columns) = &cte.columns
        {
            table_ref.columns = Some(columns.clone());
        }
    }
}

/// Detects CTE names from `WITH name AS (…)` / `, name AS (…)` patterns at
/// the cursor's depth. Returns them as [`TableRef`]s with no alias so that
/// downstream column resolution can at least know the CTE is in scope (even
/// if its columns aren't in the catalog).
fn detect_cte_names(tokens: &[MergedTok]) -> Vec<TableRef> {
    detect_ctes(tokens)
        .into_iter()
        .map(|cte| TableRef {
            name: cte.name,
            alias: None,
            columns: None,
        })
        .collect()
}

/// A common table expression declared in a `WITH` list.
#[derive(Debug, Clone)]
pub(crate) struct CteDecl {
    pub(crate) name: String,
    /// The explicit `name (a, b)` list when given, otherwise the body's
    /// select list; `None` when neither yields names.
    pub(crate) columns: Option<Vec<String>>,
    /// Document-absolute byte offset of the CTE's name.
    pub(crate) start: usize,
}

/// The CTEs the cursor can reference as a FROM / JOIN source: those declared
/// in the caret's own statement, at its depth or in an enclosing scope,
/// whose declaration starts before the cursor. A statement's CTEs are not
/// visible to the statements after it, and a CTE cannot be read before its
/// `WITH` list declares it (feedback ifb_b360).
pub(crate) fn query_cte_names(sql: &str, cursor: usize) -> Vec<String> {
    let cursor = cursor.min(sql.len());
    let (tokens, depth) = statement_tokens(sql, cursor);
    let mut names: Vec<String> = Vec::new();
    for level in (0..=depth).rev() {
        for cte in detect_ctes(&depth_merged_tokens_at(&tokens, level)) {
            if cte.start < cursor && !names.iter().any(|n| n.eq_ignore_ascii_case(&cte.name)) {
                names.push(cte.name);
            }
        }
    }
    names
}

/// Each CTE declared at this depth with its output columns: the explicit
/// `name (a, b) AS (…)` list when given, otherwise the body's select list.
fn detect_ctes(tokens: &[MergedTok]) -> Vec<CteDecl> {
    let mut refs = Vec::new();
    let mut idx = 0;
    let mut found_with = false;
    while idx < tokens.len() {
        let word = tokens[idx].text.as_str();
        if word.eq_ignore_ascii_case("WITH") {
            found_with = true;
            idx += 1;
            break;
        }
        if word.eq_ignore_ascii_case("SELECT")
            || word.eq_ignore_ascii_case("INSERT")
            || word.eq_ignore_ascii_case("UPDATE")
            || word.eq_ignore_ascii_case("DELETE")
        {
            return refs;
        }
        idx += 1;
    }
    if !found_with {
        return refs;
    }

    while idx < tokens.len() {
        let word = tokens[idx].text.as_str();
        if word.eq_ignore_ascii_case("SELECT")
            || word.eq_ignore_ascii_case("INSERT")
            || word.eq_ignore_ascii_case("UPDATE")
            || word.eq_ignore_ascii_case("DELETE")
        {
            break;
        }

        let list = tokens
            .get(idx + 1)
            .filter(|t| t.text == CTE_COLUMN_LIST)
            .and_then(|t| t.derived.clone());
        let as_idx = idx + 1 + usize::from(list.is_some());
        if as_idx < tokens.len()
            && tokens[as_idx].text.as_str().eq_ignore_ascii_case("AS")
            && !is_reserved_alias(tokens[idx].text.as_str())
            && !tokens[idx].text.as_str().eq_ignore_ascii_case("WITH")
            && !tokens[idx].text.as_str().eq_ignore_ascii_case("RECURSIVE")
        {
            let body = tokens
                .get(as_idx + 1)
                .filter(|t| t.text == DERIVED_TABLE)
                .and_then(|t| t.derived.clone());
            refs.push(CteDecl {
                name: tokens[idx].text.clone(),
                columns: list.or(body),
                start: tokens[idx].start,
            });
            idx = as_idx + 1;
            continue;
        }

        idx += 1;
    }

    refs
}

pub(crate) fn is_reserved_alias(word: &str) -> bool {
    word.eq_ignore_ascii_case("ON")
        || word.eq_ignore_ascii_case("WHERE")
        || word.eq_ignore_ascii_case("JOIN")
        || word.eq_ignore_ascii_case("LEFT")
        || word.eq_ignore_ascii_case("RIGHT")
        || word.eq_ignore_ascii_case("INNER")
        || word.eq_ignore_ascii_case("OUTER")
        || word.eq_ignore_ascii_case("FULL")
        || word.eq_ignore_ascii_case("CROSS")
        || word.eq_ignore_ascii_case("GROUP")
        || word.eq_ignore_ascii_case("ORDER")
        || word.eq_ignore_ascii_case("HAVING")
        || word.eq_ignore_ascii_case("BY")
        || word.eq_ignore_ascii_case("UNION")
        || word.eq_ignore_ascii_case("LIMIT")
        || word.eq_ignore_ascii_case("OFFSET")
        || word.eq_ignore_ascii_case("SET")
        || word.eq_ignore_ascii_case("VALUES")
}

/// A scanned token with dotted identifiers (`Sales.Orders`,
/// `db.schema.table`) merged back into a single text string. The scanner
/// emits dots as separate `Dot` tokens; this type represents the post-merge
/// view where `Sales.Orders` is one unit.
#[derive(Debug, Clone)]
struct MergedTok {
    text: String,
    is_comma: bool,
    /// Set on the single token standing in for a parenthesized derived table
    /// (`(SELECT …)`): the columns its select list produces.
    derived: Option<Vec<String>>,
    /// Document-absolute byte offset where the token starts.
    start: usize,
}

/// Text of the token that stands in for a derived table in the merged stream.
const DERIVED_TABLE: &str = "(subquery)";

/// Text of the token that stands in for a CTE's explicit column list
/// (`WITH name (a, b) AS (…)`); its `derived` holds the names.
const CTE_COLUMN_LIST: &str = "(columns)";

/// Returns `true` for SQL keywords that should never be merged across a dot.
/// Prevents `o.FROM` or `FROM.Sales` from being joined into a single token
/// that would hide the keyword from clause/context detection.
pub(crate) fn looks_like_keyword(text: &str) -> bool {
    text.eq_ignore_ascii_case("SELECT")
        || text.eq_ignore_ascii_case("FROM")
        || text.eq_ignore_ascii_case("WHERE")
        || text.eq_ignore_ascii_case("JOIN")
        || text.eq_ignore_ascii_case("ON")
        || text.eq_ignore_ascii_case("AS")
        || text.eq_ignore_ascii_case("AND")
        || text.eq_ignore_ascii_case("OR")
        || text.eq_ignore_ascii_case("GROUP")
        || text.eq_ignore_ascii_case("ORDER")
        || text.eq_ignore_ascii_case("BY")
        || text.eq_ignore_ascii_case("HAVING")
        || text.eq_ignore_ascii_case("SET")
        || text.eq_ignore_ascii_case("INSERT")
        || text.eq_ignore_ascii_case("UPDATE")
        || text.eq_ignore_ascii_case("DELETE")
        || text.eq_ignore_ascii_case("WITH")
        || text.eq_ignore_ascii_case("INTO")
        || text.eq_ignore_ascii_case("VALUES")
        || text.eq_ignore_ascii_case("UNION")
        || text.eq_ignore_ascii_case("INNER")
        || text.eq_ignore_ascii_case("LEFT")
        || text.eq_ignore_ascii_case("RIGHT")
        || text.eq_ignore_ascii_case("FULL")
        || text.eq_ignore_ascii_case("CROSS")
        || text.eq_ignore_ascii_case("OUTER")
        || text.eq_ignore_ascii_case("NOT")
        || text.eq_ignore_ascii_case("IN")
        || text.eq_ignore_ascii_case("LIKE")
        || text.eq_ignore_ascii_case("BETWEEN")
        || text.eq_ignore_ascii_case("IS")
        || text.eq_ignore_ascii_case("NULL")
        || text.eq_ignore_ascii_case("ALL")
        || text.eq_ignore_ascii_case("DISTINCT")
        || text.eq_ignore_ascii_case("TOP")
        || text.eq_ignore_ascii_case("CASE")
        || text.eq_ignore_ascii_case("WHEN")
        || text.eq_ignore_ascii_case("THEN")
        || text.eq_ignore_ascii_case("ELSE")
        || text.eq_ignore_ascii_case("END")
}

/// Returns merged word strings at the cursor's paren depth, optionally
/// limited to tokens before the cursor. Consecutive `Word . Word` sequences
/// are joined into dotted identifiers (`Sales.Orders`) so that
/// schema-qualified names are treated as a single unit — matching the
/// behavior of the old `split_whitespace()` approach.
pub(crate) fn depth_words(sql: &str, cursor: usize, before_only: bool) -> Vec<String> {
    let (tokens, depth) = statement_tokens(sql, cursor);

    let filtered: Vec<&scan::ScanToken<'_>> = tokens
        .iter()
        .filter(|t| t.paren_depth == depth)
        .filter(|t| {
            if before_only {
                t.byte_offset < cursor.min(sql.len())
            } else {
                true
            }
        })
        .filter(|t| matches!(t.kind, scan::ScanTokenKind::Word | scan::ScanTokenKind::Dot))
        .collect();

    let mut words = Vec::new();
    let mut i = 0;
    while i < filtered.len() {
        if filtered[i].kind == scan::ScanTokenKind::Word {
            let mut text = filtered[i].text.as_str().to_string();
            while i + 2 < filtered.len()
                && filtered[i + 1].kind == scan::ScanTokenKind::Dot
                && filtered[i + 2].kind == scan::ScanTokenKind::Word
                && !looks_like_keyword(filtered[i].text.as_str())
                && !looks_like_keyword(filtered[i + 2].text.as_str())
            {
                text.push('.');
                text.push_str(filtered[i + 2].text.as_str());
                i += 2;
            }
            words.push(text);
        }
        i += 1;
    }
    words
}

/// Returns merged tokens (words + commas) at `depth` from the full scanned
/// token stream. Used by `query_table_refs` to see table definitions that
/// appear after the cursor (fill-in-the-middle). A parenthesized derived
/// table at `depth` (`FROM (SELECT …) t`) collapses into one
/// [`DERIVED_TABLE`] token carrying its output columns: depth filtering
/// alone would drop the parentheses and their contents, leaving `FROM t`
/// — the alias read as a table name (feedback ifb_8e1f).
fn depth_merged_tokens_at(tokens: &[scan::ScanToken<'_>], depth: usize) -> Vec<MergedTok> {
    enum Item<'t, 'a> {
        Tok(&'t scan::ScanToken<'a>),
        Derived(Vec<String>, usize),
        ColumnList(Vec<String>, usize),
    }
    let mut filtered: Vec<Item<'_, '_>> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.paren_depth == depth
            && t.kind == scan::ScanTokenKind::LeftParen
            && let Some(close) = derived_table_close(tokens, i, depth)
        {
            filtered.push(Item::Derived(
                derived_columns(&tokens[i + 1..close], depth + 1),
                t.byte_offset,
            ));
            i = close + 1;
            continue;
        }
        if t.paren_depth == depth
            && t.kind == scan::ScanTokenKind::LeftParen
            && let Some(close) = cte_column_list_close(tokens, i, depth)
        {
            filtered.push(Item::ColumnList(
                tokens[i + 1..close]
                    .iter()
                    .filter(|t| t.kind == scan::ScanTokenKind::Word)
                    .map(|t| t.text.as_str().to_string())
                    .collect(),
                t.byte_offset,
            ));
            i = close + 1;
            continue;
        }
        if t.paren_depth == depth
            && matches!(
                t.kind,
                scan::ScanTokenKind::Word | scan::ScanTokenKind::Comma | scan::ScanTokenKind::Dot
            )
        {
            filtered.push(Item::Tok(t));
        }
        i += 1;
    }

    fn word<'i>(item: &'i Item<'_, '_>) -> Option<&'i str> {
        match item {
            Item::Tok(t) if t.kind == scan::ScanTokenKind::Word => Some(t.text.as_str()),
            _ => None,
        }
    }

    fn is_dot(item: &Item<'_, '_>) -> bool {
        matches!(item, Item::Tok(t) if t.kind == scan::ScanTokenKind::Dot)
    }

    let mut result = Vec::new();
    let mut i = 0;
    while i < filtered.len() {
        match &filtered[i] {
            Item::Derived(columns, start) => result.push(MergedTok {
                text: DERIVED_TABLE.to_string(),
                is_comma: false,
                derived: Some(columns.clone()),
                start: *start,
            }),
            Item::ColumnList(columns, start) => result.push(MergedTok {
                text: CTE_COLUMN_LIST.to_string(),
                is_comma: false,
                derived: Some(columns.clone()),
                start: *start,
            }),
            Item::Tok(t) if t.kind == scan::ScanTokenKind::Word => {
                let start = t.byte_offset;
                let mut text = t.text.as_str().to_string();
                while i + 2 < filtered.len()
                    && is_dot(&filtered[i + 1])
                    && let (Some(head), Some(next)) = (word(&filtered[i]), word(&filtered[i + 2]))
                    && !looks_like_keyword(head)
                    && !looks_like_keyword(next)
                {
                    text.push('.');
                    text.push_str(next);
                    i += 2;
                }
                result.push(MergedTok {
                    text,
                    is_comma: false,
                    derived: None,
                    start,
                });
            }
            Item::Tok(t) if t.kind == scan::ScanTokenKind::Comma => {
                result.push(MergedTok {
                    text: ",".to_string(),
                    is_comma: true,
                    derived: None,
                    start: t.byte_offset,
                });
            }
            Item::Tok(_) => {}
        }
        i += 1;
    }
    result
}

/// When the `(` at `open` begins a derived table — a query (`SELECT` or a
/// `WITH`-led one) in a FROM / JOIN source position, or a CTE body after
/// `name AS` — the index of its matching `)`.
fn derived_table_close(tokens: &[scan::ScanToken<'_>], open: usize, depth: usize) -> Option<usize> {
    let first = tokens.get(open + 1)?;
    if first.kind != scan::ScanTokenKind::Word
        || !(first.text.as_str().eq_ignore_ascii_case("SELECT")
            || first.text.as_str().eq_ignore_ascii_case("WITH"))
    {
        return None;
    }
    let source_position = tokens[..open]
        .iter()
        .rev()
        .find(|t| t.paren_depth == depth && t.kind != scan::ScanTokenKind::Other)
        .is_some_and(|t| {
            t.kind == scan::ScanTokenKind::Comma
                || (t.kind == scan::ScanTokenKind::Word
                    && (t.text.as_str().eq_ignore_ascii_case("FROM")
                        || t.text.as_str().eq_ignore_ascii_case("JOIN")
                        || t.text.as_str().eq_ignore_ascii_case("APPLY")))
        })
        || in_with_list(tokens, open, depth);
    if !source_position {
        return None;
    }
    tokens[open + 1..]
        .iter()
        .position(|t| t.paren_depth == depth && t.kind == scan::ScanTokenKind::RightParen)
        .map(|offset| open + 1 + offset)
}

/// Whether `open` sits in a `WITH` list — after its scope's leading `WITH`
/// and before the main query's first statement keyword — where a `(` is a
/// CTE's column list or body rather than a call or a table function.
fn in_with_list(tokens: &[scan::ScanToken<'_>], open: usize, depth: usize) -> bool {
    let scope_start = tokens[..open]
        .iter()
        .rposition(|t| t.paren_depth < depth)
        .map_or(0, |i| i + 1);
    let mut words = tokens[scope_start..open]
        .iter()
        .filter(|t| t.paren_depth == depth && t.kind == scan::ScanTokenKind::Word);
    if !words
        .next()
        .is_some_and(|t| t.text.as_str().eq_ignore_ascii_case("WITH"))
    {
        return false;
    }
    !words.any(|t| {
        ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE"]
            .iter()
            .any(|w| t.text.as_str().eq_ignore_ascii_case(w))
    })
}

/// When the `(` at `open` is a CTE's column list — `name (a, b) AS (` — the
/// index of its matching `)`: a name before it, only names and commas inside,
/// and `AS` right after.
fn cte_column_list_close(
    tokens: &[scan::ScanToken<'_>],
    open: usize,
    depth: usize,
) -> Option<usize> {
    if !in_with_list(tokens, open, depth) {
        return None;
    }
    let before = tokens[..open]
        .iter()
        .rev()
        .find(|t| t.paren_depth == depth && t.kind != scan::ScanTokenKind::Other)?;
    if before.kind != scan::ScanTokenKind::Word || looks_like_keyword(before.text.as_str()) {
        return None;
    }
    let close = tokens[open + 1..]
        .iter()
        .position(|t| t.paren_depth == depth && t.kind == scan::ScanTokenKind::RightParen)
        .map(|offset| open + 1 + offset)?;
    let inner_ok = tokens[open + 1..close].iter().all(|t| {
        matches!(
            t.kind,
            scan::ScanTokenKind::Word | scan::ScanTokenKind::Comma | scan::ScanTokenKind::Other
        )
    });
    let after = tokens[close + 1..]
        .iter()
        .find(|t| t.paren_depth == depth && t.kind != scan::ScanTokenKind::Other)?;
    (inner_ok
        && close > open + 1
        && after.kind == scan::ScanTokenKind::Word
        && after.text.as_str().eq_ignore_ascii_case("AS"))
    .then_some(close)
}

/// The output column names of the query in `inner` (a derived table's body,
/// whose own select list sits at `depth`): each select item's `AS` alias,
/// `name = expr` target, implicit trailing alias, or — for a plain column
/// reference — its last name part. Items with no derivable name (`*`, an
/// unaliased expression) are skipped.
fn derived_columns(inner: &[scan::ScanToken<'_>], depth: usize) -> Vec<String> {
    let is_word = |t: &scan::ScanToken<'_>, w: &str| {
        t.kind == scan::ScanTokenKind::Word && t.text.as_str().eq_ignore_ascii_case(w)
    };
    // The outermost SELECT at this depth; a WITH-led body's CTE queries sit
    // deeper, so they are skipped.
    let Some(select) = inner
        .iter()
        .position(|t| t.paren_depth == depth && is_word(t, "SELECT"))
    else {
        return Vec::new();
    };
    let end = inner[select + 1..]
        .iter()
        .position(|t| {
            t.paren_depth == depth
                && ["FROM", "INTO", "WHERE", "UNION", "EXCEPT", "INTERSECT"]
                    .iter()
                    .any(|w| is_word(t, w))
        })
        .map_or(inner.len(), |offset| select + 1 + offset);

    let mut columns: Vec<String> = Vec::new();
    let mut item: Vec<&scan::ScanToken<'_>> = Vec::new();
    let mut first_item = true;
    let mut flush = |item: &mut Vec<&scan::ScanToken<'_>>, first: bool| {
        let mut level: Vec<&scan::ScanToken<'_>> = item
            .iter()
            .copied()
            .filter(|t| t.paren_depth == depth)
            .collect();
        if first {
            // Leading DISTINCT / ALL / TOP n / TOP (n) belong to the SELECT.
            loop {
                match level.first() {
                    Some(t) if is_word(t, "DISTINCT") || is_word(t, "ALL") => {
                        level.remove(0);
                    }
                    Some(t) if is_word(t, "TOP") => {
                        level.remove(0);
                        if level
                            .first()
                            .is_some_and(|t| t.kind == scan::ScanTokenKind::LeftParen)
                        {
                            level.drain(..2.min(level.len()));
                        } else if !level.is_empty() {
                            level.remove(0);
                        }
                        if level.first().is_some_and(|t| is_word(t, "PERCENT")) {
                            level.remove(0);
                        }
                    }
                    _ => break,
                }
            }
        }
        item.clear();
        if let Some(name) = select_item_name(&level)
            && !columns.iter().any(|c| c.eq_ignore_ascii_case(&name))
        {
            columns.push(name);
        }
    };
    for t in &inner[select + 1..end] {
        if t.paren_depth == depth && t.kind == scan::ScanTokenKind::Comma {
            flush(&mut item, first_item);
            first_item = false;
        } else {
            item.push(t);
        }
    }
    flush(&mut item, first_item);
    columns
}

/// The output name of one select item, from its tokens at the select list's
/// own depth (nested parentheses are already removed).
fn select_item_name(level: &[&scan::ScanToken<'_>]) -> Option<String> {
    use scan::ScanTokenKind::{Dot, Other, RightParen, Word};
    let word = |t: &scan::ScanToken<'_>| t.kind == Word;
    // `name = expr` (T-SQL).
    if level.len() >= 3 && word(level[0]) && level[1].kind == Other && level[1].text.as_str() == "="
    {
        return Some(level[0].text.as_str().to_string());
    }
    let last = level.last().copied()?;
    if !word(last) || looks_like_keyword(last.text.as_str()) {
        return None;
    }
    let n = level.len();
    if n == 1 {
        return Some(last.text.as_str().to_string());
    }
    let prev = level[n - 2];
    // `expr AS name`.
    if word(prev) && prev.text.as_str().eq_ignore_ascii_case("AS") {
        return Some(last.text.as_str().to_string());
    }
    // A plain column reference `a.b.c`: its last part.
    let dotted = level
        .iter()
        .enumerate()
        .all(|(i, t)| if i % 2 == 0 { word(t) } else { t.kind == Dot });
    if dotted && n % 2 == 1 {
        return Some(last.text.as_str().to_string());
    }
    // `expr name` with the AS left out: a word right after a complete operand.
    let operand_end = prev.kind == RightParen
        || (word(prev) && !looks_like_keyword(prev.text.as_str()))
        || (word(prev) && prev.text.as_str().eq_ignore_ascii_case("END"));
    if operand_end && n >= 2 && level[n - 2].kind != Dot {
        return Some(last.text.as_str().to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- CTE sources (DBeaver #42293's query) ------------------------

    fn source<'r>(refs: &'r [TableRef], name: &str) -> &'r TableRef {
        refs.iter()
            .find(|r| r.name.eq_ignore_ascii_case(name) && r.alias.is_some())
            .unwrap_or_else(|| panic!("no aliased {name} in {refs:?}"))
    }

    #[test]
    fn cte_sources_carry_the_ctes_columns() {
        let sql = "WITH a AS (SELECT x AS one, y two FROM t), b (p, q) AS (SELECT 1, 2)\nSELECT * FROM a AS aa JOIN b bb ON 1 = 1";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(
            source(&refs, "a").columns.as_deref(),
            Some(&["one".to_string(), "two".to_string()][..])
        );
        assert_eq!(
            source(&refs, "b").columns.as_deref(),
            Some(&["p".to_string(), "q".to_string()][..])
        );
        // The CTE names themselves stay column-less, so an unreferenced CTE
        // never feeds the main query's column list.
        assert!(
            refs.iter()
                .filter(|r| r.alias.is_none())
                .all(|r| r.columns.is_none())
        );
    }

    #[test]
    fn a_cte_body_sees_an_earlier_ctes_columns() {
        let sql = "WITH d AS (SELECT 1 AS dt), e AS (SELECT EXTRACT(YEAR FROM cal.dt) AS y, cal. FROM d AS cal) SELECT * FROM e";
        let cursor = sql.find("cal. ").unwrap() + 4;
        let refs = crate::intellisense::complete::query_local::visible_aliases(sql, cursor);
        assert_eq!(
            source(&refs, "d").columns.as_deref(),
            Some(&["dt".to_string()][..])
        );
    }

    #[test]
    fn calls_and_table_functions_before_as_are_not_cte_lists() {
        let sql = "SELECT COUNT(x) AS n FROM dbo.fn(1, 2) AS f JOIN t2 AS g ON 1 = 1";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(source(&refs, "dbo.fn").alias.as_deref(), Some("f"));
        assert!(source(&refs, "dbo.fn").columns.is_none());
        assert_eq!(source(&refs, "t2").alias.as_deref(), Some("g"));
    }

    // ---- Clause context detection tests -------------------------------

    #[test]
    fn clause_context_select_list() {
        assert_eq!(
            detect_clause_context("SELECT CustomerId, Order", 23),
            ClauseContext::SelectList
        );
        assert_eq!(
            detect_clause_context("SELECT ", 7),
            ClauseContext::SelectList
        );
    }

    #[test]
    fn clause_context_from_clause() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Sales.Orders ", 27),
            ClauseContext::FromClause
        );
    }

    #[test]
    fn clause_context_where_clause() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders WHERE Customer", 34),
            ClauseContext::WhereClause
        );
    }

    #[test]
    fn clause_context_group_by_clause() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders GROUP BY Cust", 33),
            ClauseContext::GroupByClause
        );
    }

    #[test]
    fn clause_context_order_by_clause() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders ORDER BY Cust", 33),
            ClauseContext::OrderByClause
        );
    }

    #[test]
    fn clause_context_join_target() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders JOIN Cust", 29),
            ClauseContext::JoinTarget
        );
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders INNER JOIN Cust", 35),
            ClauseContext::JoinTarget
        );
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders LEFT JOIN Cust", 34),
            ClauseContext::JoinTarget
        );
    }

    #[test]
    fn clause_context_join_on() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders o JOIN Customers c ON o.Cust", 47),
            ClauseContext::JoinOn
        );
    }

    #[test]
    fn clause_context_having_clause() {
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders GROUP BY Cust HAVING Count", 43),
            ClauseContext::HavingClause
        );
    }

    #[test]
    fn clause_context_set_clause() {
        assert_eq!(
            detect_clause_context("UPDATE Orders SET Cust", 21),
            ClauseContext::SetClause
        );
    }

    #[test]
    fn clause_context_unknown() {
        assert_eq!(detect_clause_context("", 0), ClauseContext::Unknown);
        assert_eq!(detect_clause_context("xyz", 3), ClauseContext::Unknown);
    }

    #[test]
    fn join_keyword_exact_match_not_suffix() {
        // "DISJOIN" ends with "JOIN" but is not a JOIN keyword.
        assert_eq!(
            detect_clause_context("SELECT * FROM Orders DISJOIN ", 28),
            ClauseContext::FromClause
        );
    }

    #[test]
    fn set_without_update_not_set_clause() {
        assert_ne!(
            detect_clause_context("SET @var = 1", 5),
            ClauseContext::SetClause
        );
    }

    #[test]
    fn set_after_update_is_set_clause() {
        assert_eq!(
            detect_clause_context("UPDATE Orders SET Cust", 21),
            ClauseContext::SetClause
        );
    }

    #[test]
    fn set_after_select_not_set_clause() {
        // SET after SELECT (not valid SQL, but should not be SetClause).
        assert_ne!(
            detect_clause_context("SELECT col SET x", 14),
            ClauseContext::SetClause
        );
    }

    // ---- Table ref extraction tests -----------------------------------

    #[test]
    fn table_refs_extracts_from_and_join() {
        let sql = "SELECT * FROM Orders o JOIN Customers c \
                   ON o.CustomerId = c.CustomerId";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "Orders");
        assert_eq!(refs[0].alias.as_deref(), Some("o"));
        assert_eq!(refs[1].name, "Customers");
        assert_eq!(refs[1].alias.as_deref(), Some("c"));
    }

    #[test]
    fn table_refs_extracts_table_without_alias() {
        let sql = "SELECT * FROM Orders WHERE ";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "Orders");
        assert!(refs[0].alias.is_none());
    }

    #[test]
    fn table_refs_extracts_with_as_keyword() {
        let sql = "SELECT * FROM Orders AS o WHERE ";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "Orders");
        assert_eq!(refs[0].alias.as_deref(), Some("o"));
    }

    #[test]
    fn table_refs_extracts_comma_joined_tables() {
        let sql = "SELECT * FROM Orders o, Customers c WHERE ";
        let refs = query_table_refs(sql, sql.len());
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "Orders");
        assert_eq!(refs[0].alias.as_deref(), Some("o"));
        assert_eq!(refs[1].name, "Customers");
        assert_eq!(refs[1].alias.as_deref(), Some("c"));
    }

    #[test]
    fn cte_name_detected_as_table_ref() {
        let sql = "WITH cte AS (SELECT a FROM HR.Department) SELECT * FROM cte WHERE ";
        let refs = query_table_refs(sql, sql.len());
        assert!(
            refs.iter().any(|r| r.name == "cte"),
            "CTE name 'cte' should be detected as a table ref, got {:?}",
            refs.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cte_multiple_names_detected() {
        let sql = "WITH a AS (SELECT x FROM t1), b AS (SELECT y FROM t2) \
                   SELECT * FROM a JOIN b ON a.x = b.y WHERE ";
        let refs = query_table_refs(sql, sql.len());
        assert!(refs.iter().any(|r| r.name == "a"), "should detect CTE 'a'");
        assert!(refs.iter().any(|r| r.name == "b"), "should detect CTE 'b'");
    }

    #[test]
    fn join_awaiting_on_needs_a_source_and_alias_and_no_condition() {
        let at = |sql: &str| {
            let cursor = sql.find('|').unwrap();
            join_awaiting_on(&sql.replace('|', ""), cursor)
        };
        assert!(at("SELECT * FROM a JOIN dbo.T AS t |"));
        assert!(at("SELECT * FROM a JOIN dbo.T t\n\t|\nWHERE x = 1"));
        assert!(at("SELECT * FROM a JOIN dbo.T t f|"));
        // Still typing the alias, or no alias yet.
        assert!(!at("SELECT * FROM a JOIN dbo.T t|"));
        assert!(!at("SELECT * FROM a JOIN dbo.T |"));
        // The condition is already under way.
        assert!(!at("SELECT * FROM a JOIN dbo.T t ON |"));
        assert!(!at("SELECT * FROM a JOIN dbo.T t t.x = |"));
        assert!(!at("SELECT * FROM a |"));
    }

    #[test]
    fn join_awaiting_on_skips_joins_that_take_no_condition() {
        let at = |sql: &str| {
            let cursor = sql.find('|').unwrap();
            join_awaiting_on(&sql.replace('|', ""), cursor)
        };
        // CROSS / NATURAL joins have no ON: the clause is complete.
        assert!(!at("SELECT * FROM a CROSS JOIN dbo.T t |"));
        assert!(!at("SELECT * FROM a cross join dbo.T AS t WH|"));
        assert!(!at("SELECT * FROM a NATURAL JOIN dbo.T t |"));
        assert!(!at("SELECT * FROM a NATURAL LEFT JOIN dbo.T t |"));
        assert!(!at("SELECT * FROM a NATURAL LEFT OUTER JOIN dbo.T t |"));
        assert!(!at("SELECT * FROM a NATURAL INNER JOIN dbo.T t WH|"));
        // Conditioned joins still wait for their ON.
        assert!(at("SELECT * FROM a LEFT OUTER JOIN dbo.T t |"));
        assert!(at("SELECT * FROM a INNER JOIN dbo.T t |"));
        assert!(at("SELECT * FROM a FULL JOIN dbo.T t |"));
        // Only the current join counts, not an earlier CROSS JOIN.
        assert!(at("SELECT * FROM a CROSS JOIN b INNER JOIN dbo.T t |"));
        assert!(!at(
            "SELECT * FROM a INNER JOIN b ON b.id = a.id CROSS JOIN dbo.T t |"
        ));
    }
}
