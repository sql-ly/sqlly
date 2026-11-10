//! IDENTITY_INSERT lint for `INSERT` statements on engines that gate
//! explicit identity values behind `SET IDENTITY_INSERT` (the SQL Server
//! family — other engines either have no such switch or use different
//! mechanisms, and produce no findings here).
//!
//! Pure mapping like `insert_hints`: walk the buffer's statements in order,
//! track the `SET IDENTITY_INSERT <table> ON|OFF` state each statement
//! leaves behind, and flag any INSERT that supplies a value for an identity
//! column while the switch is off for its table. Identity columns come from
//! the schema snapshot; unknown tables never warn.

use std::collections::HashMap;

use sqlly_intel::query::tokenizer::{Token, TokenKind, tokenize};
use sqlly_lang::sqlmodel::{Dialect, ModelSnapshot, ObjectKind, ObjectRef, SqlName};

use super::insert_hints::{may_contain_insert, split_table_ref};
use super::statements::statement_spans_for;

/// One warning: underline `start..end` (buffer byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IdentityLintFinding {
    pub start: usize,
    pub end: usize,
    /// The table as the user spelled it — used as the diagnostic identifier.
    pub table: String,
    pub message: String,
    /// Whether the INSERT names the identity column in an explicit column
    /// list. Only those can be fixed by flipping the switch alone — a
    /// positional INSERT also needs a column list, which SQL Server requires
    /// whenever IDENTITY_INSERT is ON.
    pub column_listed: bool,
    /// The offending statement, trimmed to its first and last non-blank
    /// bytes. The quick fix wraps exactly this range.
    pub stmt_start: usize,
    pub stmt_end: usize,
}

/// The right-click fix for a `column_listed` finding: the whole buffer with
/// `SET IDENTITY_INSERT <table> ON;` / `… OFF;` wrapped around the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IdentityInsertFix {
    pub sql: String,
    /// Where the caret lands in `sql` — the same spot it was in before.
    pub cursor: usize,
    /// Table spelling, for the menu label.
    pub table: String,
}

/// Last `SET IDENTITY_INSERT` state per table: name fold → (schema fold if
/// the SET statement qualified one, is-on).
type IdentityState = HashMap<String, (Option<String>, bool)>;

pub(super) fn identity_insert_warnings(
    content: &str,
    dialect: Dialect,
    snapshot: &ModelSnapshot,
) -> Vec<IdentityLintFinding> {
    if dialect != Dialect::SqlServer || !may_contain_insert(content) {
        return Vec::new();
    }
    let mut state = IdentityState::new();
    let mut out = Vec::new();
    for span in statement_spans_for(content, dialect) {
        if span.end <= span.start || span.end > content.len() {
            continue;
        }
        let slice = &content[span.start..span.end];
        // Spans tile the buffer, so a span carries the blank run that
        // separates it from its neighbours. Trim to the statement itself —
        // that is what the quick fix wraps.
        let stmt_start = span.start + (slice.len() - slice.trim_start().len());
        let stmt_end = span.end - (slice.len() - slice.trim_end().len());
        let tokens: Vec<Token<'_>> = tokenize(slice)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace && t.kind != TokenKind::Comment)
            .collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        if is_word(first, "set") {
            record_set_identity_insert(&tokens, dialect, &mut state);
        } else if is_word(first, "insert") {
            check_insert(
                &tokens,
                span.start,
                (stmt_start, stmt_end),
                dialect,
                snapshot,
                &state,
                &mut out,
            );
        }
    }
    out
}

/// `SET IDENTITY_INSERT <table> ON|OFF` → update the per-table state.
/// Anything else starting with SET is ignored.
fn record_set_identity_insert(tokens: &[Token<'_>], dialect: Dialect, state: &mut IdentityState) {
    if tokens.len() < 3 || !is_word(&tokens[1], "identity_insert") {
        return;
    }
    let Some((raw, next, _, _)) = take_object_name(tokens, 2, &["on", "off"]) else {
        return;
    };
    let Some(switch) = tokens.get(next) else {
        return;
    };
    let on = eq_ci(switch.text, "on");
    if !on && !eq_ci(switch.text, "off") {
        return;
    }
    let (schema, name) = split_table_ref(&raw);
    state.insert(
        fold(&name, dialect),
        (schema.map(|s| fold(&s, dialect)), on),
    );
}

fn check_insert(
    tokens: &[Token<'_>],
    base: usize,
    stmt: (usize, usize),
    dialect: Dialect,
    snapshot: &ModelSnapshot,
    state: &IdentityState,
    out: &mut Vec<IdentityLintFinding>,
) {
    let mut i = 1;
    if i < tokens.len() && is_word(&tokens[i], "into") {
        i += 1;
    }
    let stops = ["values", "output", "default", "select", "with", "set"];
    let Some((raw, next, table_start, table_end)) = take_object_name(tokens, i, &stops) else {
        return;
    };
    i = next;
    // Table hints (`WITH (TABLOCK)`): skip the parenthesized group so the
    // token after it decides between a column list and a positional insert.
    if i < tokens.len() && is_word(&tokens[i], "with") {
        i += 1;
        if tokens.get(i).is_some_and(|t| t.text == "(") {
            i = skip_paren_group(tokens, i);
        }
    }

    let (schema, name) = split_table_ref(&raw);
    let reference = ObjectRef::new(
        None,
        schema.clone().map(|s| SqlName::new(s, dialect, false)),
        SqlName::new(name.clone(), dialect, false),
    );
    let Some(entry) = snapshot.resolve_kind(&reference, ObjectKind::Table, None, None) else {
        return;
    };
    let Some(table_body) = entry.table() else {
        return;
    };
    let Some(identity_col) = table_body.columns.iter().find(|c| c.identity) else {
        return;
    };
    if identity_insert_is_on(state, dialect, schema.as_deref(), &name) {
        return;
    }

    let col = identity_col.name.spelling();
    match tokens.get(i) {
        // Explicit column list: warn only when the identity column is listed.
        Some(t) if t.text == "(" => {
            if let Some((cstart, cend)) = column_list_find(tokens, i, &identity_col.name, dialect) {
                out.push(IdentityLintFinding {
                    start: base + cstart,
                    end: base + cend,
                    table: raw.clone(),
                    message: format!(
                        "IDENTITY_INSERT is OFF: '{col}' is an identity column of {raw}. \
                         Run SET IDENTITY_INSERT {raw} ON before this INSERT (and OFF after), \
                         or remove the column and its value. Right-click '{col}' to add the \
                         SET statements around this INSERT."
                    ),
                    column_listed: true,
                    stmt_start: stmt.0,
                    stmt_end: stmt.1,
                });
            }
        }
        // No column list before VALUES/SELECT/OUTPUT: a positional insert
        // supplies every column, identity included.
        Some(t) if is_word(t, "values") || is_word(t, "select") || is_word(t, "output") => {
            out.push(IdentityLintFinding {
                start: base + table_start,
                end: base + table_end,
                table: raw.clone(),
                message: format!(
                    "IDENTITY_INSERT is OFF: {raw} has identity column '{col}', and an INSERT \
                     without a column list supplies a value for it. Run SET IDENTITY_INSERT \
                     {raw} ON and list the columns explicitly, or add a column list that \
                     omits '{col}'."
                ),
                column_listed: false,
                stmt_start: stmt.0,
                stmt_end: stmt.1,
            });
        }
        // DEFAULT VALUES, INSERT … SET, or a truncated statement: nothing
        // supplies the identity value.
        _ => {}
    }
}

/// The quick fix offered when the user right-clicks an identity column that
/// this lint has flagged: `None` unless `offset` sits on (or immediately
/// after) the flagged column of a fixable INSERT.
pub(super) fn identity_insert_fix_at(
    content: &str,
    dialect: Dialect,
    snapshot: &ModelSnapshot,
    offset: usize,
) -> Option<IdentityInsertFix> {
    let finding = identity_insert_warnings(content, dialect, snapshot)
        .into_iter()
        .find(|f| f.column_listed && offset >= f.start && offset <= f.end)?;
    Some(wrap_with_identity_insert(content, &finding, offset))
}

/// Splice `SET IDENTITY_INSERT <table> ON;` above the statement and `… OFF;`
/// below it, matching the statement's own indentation.
fn wrap_with_identity_insert(
    content: &str,
    finding: &IdentityLintFinding,
    caret: usize,
) -> IdentityInsertFix {
    let table = finding.table.clone();
    // Indent from the statement's line, but only when nothing else shares
    // the line ahead of it — `DELETE …; INSERT …` on one line gets the SET
    // spliced inline rather than dragged to the line start.
    let line_start = content[..finding.stmt_start]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    let lead = &content[line_start..finding.stmt_start];
    let (at, indent) = if lead.chars().all(|c| c == ' ' || c == '\t') {
        (line_start, lead)
    } else {
        (finding.stmt_start, "")
    };

    let prefix = format!("{indent}SET IDENTITY_INSERT {table} ON;\n");
    // An unterminated statement would otherwise run into the trailing SET.
    let terminator = if content[..finding.stmt_end].trim_end().ends_with(';') {
        ""
    } else {
        ";"
    };
    let suffix = format!("{terminator}\n{indent}SET IDENTITY_INSERT {table} OFF;");

    let mut sql = String::with_capacity(content.len() + prefix.len() + suffix.len());
    sql.push_str(&content[..at]);
    sql.push_str(&prefix);
    sql.push_str(&content[at..finding.stmt_end]);
    sql.push_str(&suffix);
    sql.push_str(&content[finding.stmt_end..]);
    IdentityInsertFix {
        sql,
        cursor: caret + prefix.len(),
        table,
    }
}

fn identity_insert_is_on(
    state: &IdentityState,
    dialect: Dialect,
    schema: Option<&str>,
    name: &str,
) -> bool {
    match state.get(&fold(name, dialect)) {
        Some((set_schema, on)) => {
            // Lenient schema match: when either the SET or the INSERT omits
            // the schema, assume they mean the same table rather than
            // producing a false warning.
            let schema_matches = match (set_schema.as_deref(), schema) {
                (Some(a), Some(b)) => a == fold(b, dialect),
                _ => true,
            };
            *on && schema_matches
        }
        None => false,
    }
}

/// Scan a flat `(col, col, …)` list starting at the `(` at `tokens[open]`;
/// return the byte span of the entry matching `identity` (offsets into the
/// tokenized statement). Nested parens mean this is not a plain column list —
/// no match is reported.
fn column_list_find(
    tokens: &[Token<'_>],
    open: usize,
    identity: &SqlName,
    dialect: Dialect,
) -> Option<(usize, usize)> {
    let mut i = open + 1;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.text == ")" {
            return None;
        }
        if t.text == "(" {
            return None;
        }
        if t.text == "," || (t.kind == TokenKind::Symbol && matches!(t.text, "\"" | "`")) {
            i += 1;
            continue;
        }
        if matches!(t.kind, TokenKind::Identifier | TokenKind::Keyword) {
            let stripped = strip_quotes(t.text);
            if SqlName::new(stripped, dialect, false).matches(identity) {
                return Some((t.start, t.end));
            }
            i += 1;
            continue;
        }
        return None;
    }
    None
}

/// Consume a (possibly schema-qualified, possibly quoted) object name from
/// `tokens[i]`, stopping at any of `stops` (compared case-insensitively
/// against the raw token text, so quoted identifiers never match) or at the
/// first non-name token. Returns the raw spelling, the next index, and the
/// byte span within the tokenized statement.
fn take_object_name(
    tokens: &[Token<'_>],
    mut i: usize,
    stops: &[&str],
) -> Option<(String, usize, usize, usize)> {
    let mut raw = String::new();
    let mut start = 0usize;
    let mut end = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if stops.iter().any(|s| eq_ci(t.text, s)) {
            break;
        }
        let name_ish = matches!(t.kind, TokenKind::Identifier | TokenKind::Keyword)
            || t.text == "."
            || (t.kind == TokenKind::Symbol && matches!(t.text, "\"" | "`"));
        if !name_ish {
            break;
        }
        if raw.is_empty() {
            start = t.start;
        }
        end = t.end;
        raw.push_str(t.text);
        i += 1;
    }
    if raw.is_empty() {
        None
    } else {
        Some((raw, i, start, end))
    }
}

/// Index just past the balanced paren group whose `(` is at `tokens[open]`.
fn skip_paren_group(tokens: &[Token<'_>], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < tokens.len() {
        match tokens[i].text {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

fn fold(name: &str, dialect: Dialect) -> String {
    SqlName::new(name, dialect, false).fold_key().to_string()
}

/// Quote-stripped identifier value, with doubled closing quotes unescaped.
fn strip_quotes(raw: &str) -> String {
    sqlly_lang::sqllex::unquote_identifier(raw).into_owned()
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_word(t: &Token<'_>, word: &str) -> bool {
    eq_ci(t.text, word)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use sqlly_lang::sqlmodel::{
        ColumnDef, Fidelity, IncompleteRegions, ObjectBody, ObjectFragment, Provenance, TableBody,
    };

    fn table_fragment(schema: &str, name: &str, identity_col: Option<&str>) -> ObjectFragment {
        let dialect = Dialect::SqlServer;
        let columns = [identity_col.unwrap_or("Id"), "Name"]
            .iter()
            .enumerate()
            .map(|(ordinal, col)| ColumnDef {
                name: SqlName::new(*col, dialect, false),
                ty: None,
                nullable: Some(ordinal != 0),
                default: None,
                identity: ordinal == 0 && identity_col.is_some(),
                generated: None,
                collation: None,
                ordinal,
            })
            .collect();
        ObjectFragment {
            reference: ObjectRef::new(
                None,
                Some(SqlName::new(schema, dialect, false)),
                SqlName::new(name, dialect, false),
            ),
            kind: ObjectKind::Table,
            dialect,
            body: ObjectBody::Table(TableBody {
                columns,
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "p1".into(),
            },
            fidelity: Fidelity::Exact,
            references: vec![],
        }
    }

    fn snapshot_with(fragment: ObjectFragment) -> ModelSnapshot {
        ModelSnapshot::empty()
            .apply(
                1,
                vec![fragment],
                &BTreeSet::new(),
                IncompleteRegions::default(),
            )
            .0
    }

    fn snapshot() -> ModelSnapshot {
        snapshot_with(table_fragment("HR", "WorkSite", Some("SiteId")))
    }

    fn warnings(sql: &str) -> Vec<IdentityLintFinding> {
        identity_insert_warnings(sql, Dialect::SqlServer, &snapshot())
    }

    #[test]
    fn positional_insert_into_identity_table_warns_on_table_name() {
        let sql = "INSERT INTO HR.WorkSite VALUES (1, 'palms')";
        let got = warnings(sql);
        assert_eq!(got.len(), 1);
        assert_eq!(&sql[got[0].start..got[0].end], "HR.WorkSite");
        assert!(got[0].message.contains("SiteId"));
        assert!(got[0].message.contains("IDENTITY_INSERT is OFF"));
    }

    #[test]
    fn column_list_with_identity_warns_on_that_column() {
        let sql = "INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        let got = warnings(sql);
        assert_eq!(got.len(), 1);
        assert_eq!(&sql[got[0].start..got[0].end], "SiteId");
    }

    #[test]
    fn column_list_without_identity_is_fine() {
        assert!(warnings("INSERT INTO HR.WorkSite (Name) VALUES ('palms')").is_empty());
    }

    #[test]
    fn preceding_set_identity_insert_on_silences() {
        let sql = "SET IDENTITY_INSERT HR.WorkSite ON;\n\
                   INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        assert!(warnings(sql).is_empty());
    }

    #[test]
    fn set_off_after_on_warns_again() {
        let sql = "SET IDENTITY_INSERT HR.WorkSite ON;\n\
                   SET IDENTITY_INSERT HR.WorkSite OFF;\n\
                   INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        assert_eq!(warnings(sql).len(), 1);
    }

    #[test]
    fn set_after_the_insert_does_not_silence() {
        let sql = "INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms');\n\
                   SET IDENTITY_INSERT HR.WorkSite ON;";
        assert_eq!(warnings(sql).len(), 1);
    }

    #[test]
    fn unqualified_set_matches_qualified_insert() {
        let sql = "SET IDENTITY_INSERT WorkSite ON;\n\
                   INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        assert!(warnings(sql).is_empty());
    }

    #[test]
    fn bracketed_names_and_case_are_folded() {
        let sql = "SET IDENTITY_INSERT [hr].[worksite] ON;\n\
                   INSERT INTO HR.WorkSite ([SITEID], Name) VALUES (1, 'palms')";
        assert!(warnings(sql).is_empty());
        let sql = "INSERT INTO [HR].[WorkSite] ([SiteId]) VALUES (1)";
        let got = warnings(sql);
        assert_eq!(got.len(), 1);
        assert_eq!(&sql[got[0].start..got[0].end], "[SiteId]");
    }

    #[test]
    fn insert_select_with_identity_in_list_warns() {
        let sql = "INSERT INTO HR.WorkSite (SiteId, Name) SELECT Id, Nm FROM Src";
        assert_eq!(warnings(sql).len(), 1);
    }

    #[test]
    fn unknown_table_default_values_and_other_dialects_are_silent() {
        assert!(warnings("INSERT INTO dbo.Nope VALUES (1)").is_empty());
        assert!(warnings("INSERT INTO HR.WorkSite DEFAULT VALUES").is_empty());
        assert!(
            identity_insert_warnings(
                "INSERT INTO HR.WorkSite VALUES (1, 'palms')",
                Dialect::Postgres,
                &snapshot(),
            )
            .is_empty()
        );
    }

    fn fix_at(sql: &str, needle: &str) -> Option<IdentityInsertFix> {
        let offset = sql.find(needle).expect("needle is in the sql");
        identity_insert_fix_at(sql, Dialect::SqlServer, &snapshot(), offset)
    }

    #[test]
    fn right_click_on_the_identity_column_wraps_the_statement() {
        let sql = "INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        let fix = fix_at(sql, "SiteId").expect("the flagged column offers a fix");
        assert_eq!(fix.table, "HR.WorkSite");
        assert_eq!(
            fix.sql,
            "SET IDENTITY_INSERT HR.WorkSite ON;\n\
             INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms');\n\
             SET IDENTITY_INSERT HR.WorkSite OFF;"
        );
        // The caret stays on the column it was clicked on.
        assert_eq!(&fix.sql[fix.cursor..fix.cursor + 6], "SiteId");
        // And the wrapped statement no longer warns.
        assert!(
            identity_insert_warnings(&fix.sql, Dialect::SqlServer, &snapshot()).is_empty(),
            "applying the fix clears the warning"
        );
    }

    #[test]
    fn the_fix_keeps_indentation_and_an_existing_terminator() {
        let sql = "    INSERT INTO HR.WorkSite (SiteId) VALUES (1);\n";
        let fix = fix_at(sql, "SiteId").expect("fix");
        assert_eq!(
            fix.sql,
            "    SET IDENTITY_INSERT HR.WorkSite ON;\n\
             \x20   INSERT INTO HR.WorkSite (SiteId) VALUES (1);\n\
             \x20   SET IDENTITY_INSERT HR.WorkSite OFF;\n"
        );
    }

    #[test]
    fn the_fix_wraps_only_the_clicked_statement() {
        let sql = "SELECT 1;\nINSERT INTO HR.WorkSite (SiteId) VALUES (1);\nSELECT 2;";
        let fix = fix_at(sql, "SiteId").expect("fix");
        assert_eq!(
            fix.sql,
            "SELECT 1;\n\
             SET IDENTITY_INSERT HR.WorkSite ON;\n\
             INSERT INTO HR.WorkSite (SiteId) VALUES (1);\n\
             SET IDENTITY_INSERT HR.WorkSite OFF;\nSELECT 2;"
        );
    }

    #[test]
    fn no_fix_off_the_flagged_column_or_on_a_positional_insert() {
        let sql = "INSERT INTO HR.WorkSite (SiteId, Name) VALUES (1, 'palms')";
        assert!(fix_at(sql, "Name").is_none(), "a plain column offers none");
        assert!(fix_at(sql, "palms").is_none(), "nor does the value");
        // Positional INSERTs need a column list too, so the switch alone is
        // not a fix — no menu entry there.
        assert!(
            fix_at("INSERT INTO HR.WorkSite VALUES (1, 'palms')", "HR").is_none(),
            "positional INSERT offers no one-click fix"
        );
        // Nothing to fix once the switch is already on.
        assert!(
            fix_at(
                "SET IDENTITY_INSERT HR.WorkSite ON;\n\
                 INSERT INTO HR.WorkSite (SiteId) VALUES (1)",
                "(SiteId"
            )
            .is_none()
        );
    }

    #[test]
    fn table_without_identity_is_silent() {
        let snap = snapshot_with(table_fragment("HR", "Plain", None));
        assert!(
            identity_insert_warnings(
                "INSERT INTO HR.Plain VALUES (1, 'x')",
                Dialect::SqlServer,
                &snap,
            )
            .is_empty()
        );
    }
}
