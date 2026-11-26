//! Completion inside the heads of DDL statements — `CREATE` / `ALTER` /
//! `DROP` / `TRUNCATE` and `DECLARE`.
//!
//! The general completion path reads every position as part of a query, so a
//! DDL head got whatever the nearest query clause would: built-in functions
//! after `ALTER TABLE ▏`, a join condition's columns after `CREATE INDEX ix
//! ON ▏`, the statement keywords where a data type belongs. Each slot of a
//! DDL head takes one specific thing, and this module names it:
//!
//! - a **new name** (`CREATE TABLE ▏`, a new column, a parameter) offers
//!   nothing but the schemas it can live in;
//! - an **existing object** (`ALTER TABLE ▏`, `DROP VIEW ▏`, `REFERENCES ▏`,
//!   `CREATE INDEX ix ON ▏`) offers the objects of that kind, never a
//!   function;
//! - a **data type** slot (`Id ▏`, `@Id ▏`, `RETURNS ▏`) offers the engine's
//!   types;
//! - a **column list** (`PRIMARY KEY (▏`, an index's keys, `DROP COLUMN ▏`)
//!   offers that table's columns;
//! - everything else offers the few keywords the grammar allows next.
//!
//! Positions that hold an expression or a query — a `DEFAULT`, a `CHECK`, a
//! computed column, a view's `SELECT`, a routine's body — are left to the
//! general path: [`ddl_items`] returns `None` for them, as it does whenever
//! it does not recognise where the caret is.

use sqlly_core::ObjectKind;
use sqlly_lang::sqlmodel::Dialect;

use super::context::statement_tokens;
use super::query_local::add_script_table_drop_items;
use super::scan::ScanTokenKind;
use super::text::current_prefix;
use crate::intellisense::builtins::builtins;
use crate::intellisense::score::{
    finalize_completion_items, fuzzy, push_match, push_scored, qualified_object_score,
    qualified_object_score_qualified, reset_push_cap_state,
};
use crate::intellisense::types::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionSource,
};

/// Every kind [`ddl_items`] can produce; callers that filter completions by
/// kind allow exactly these for a DDL slot.
pub const DDL_KINDS: [CompletionKind; 8] = [
    CompletionKind::Keyword,
    CompletionKind::Database,
    CompletionKind::Schema,
    CompletionKind::Table,
    CompletionKind::Column,
    CompletionKind::Function,
    CompletionKind::Procedure,
    CompletionKind::Type,
];

const KEYWORD_SCORE: i32 = 100;
const OBJECT_SCORE: i32 = 80;
const COLUMN_SCORE: i32 = 90;
const COMMON_TYPE_SCORE: i32 = 60;
const TYPE_SCORE: i32 = 40;

/// The completions for a caret inside a DDL statement head on `dialect`, best
/// first, or `None` when the caret is anywhere else (including the
/// expression and query parts of a DDL statement). An empty list means the
/// slot takes a name only the user can supply.
pub fn ddl_items(
    dialect: Dialect,
    sql: &str,
    cursor: usize,
    catalog: &CompletionCatalog,
) -> Option<Vec<CompletionItem>> {
    let cursor = cursor.min(sql.len());
    let prefix = current_prefix(sql, cursor);
    let prefix_start = cursor - prefix.len();
    let (scanned, _) = statement_tokens(sql, cursor);
    let first = scanned.first()?;
    if first.kind != ScanTokenKind::Word || !is_head_word(first.text.as_str()) {
        return None;
    }
    let tokens: Vec<Tok<'_>> = scanned
        .iter()
        .take_while(|t| t.end_byte <= prefix_start)
        .map(|t| Tok {
            up: t.text.as_str().to_ascii_uppercase(),
            text: t.text.as_str(),
            kind: t.kind,
            depth: t.paren_depth,
            end: t.end_byte,
        })
        .collect();
    let last_end = tokens.last()?.end;
    let cx = Cx {
        dialect,
        sql,
        prefix,
        prefix_start,
        catalog,
        fresh_line: sql[last_end..prefix_start].contains('\n'),
    };
    let offer = analyze(&cx, &tokens, 0)?;
    Some(build(&cx, cursor, offer))
}

fn is_head_word(word: &str) -> bool {
    [
        "CREATE", "ALTER", "DROP", "TRUNCATE", "DECLARE", "IF", "BEGIN", "ELSE", "WHILE",
    ]
    .iter()
    .any(|head| word.eq_ignore_ascii_case(head))
}

struct Tok<'a> {
    up: String,
    text: &'a str,
    kind: ScanTokenKind,
    depth: usize,
    end: usize,
}

struct Cx<'a> {
    dialect: Dialect,
    sql: &'a str,
    prefix: &'a str,
    prefix_start: usize,
    catalog: &'a CompletionCatalog,
    /// The caret sits on a later line than the last token: where the
    /// statement could already be complete, the user is starting the next.
    fresh_line: bool,
}

impl Cx<'_> {
    fn tsql(&self) -> bool {
        self.dialect == Dialect::SqlServer
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Table,
    View,
    Procedure,
    Function,
    Type,
}

/// What one slot offers. The default offers nothing.
#[derive(Default)]
struct Offer {
    keywords: Vec<&'static str>,
    classes: Vec<Class>,
    schemas: bool,
    databases: bool,
    /// Tables the script itself creates (`DROP TABLE ▏`).
    script_tables: bool,
    types: bool,
    columns: Vec<String>,
    /// Built-in functions and the operators of a condition: the slot holds
    /// an expression (a `DEFAULT`, a `CHECK`, a computed column).
    expression: bool,
}

impl Offer {
    fn nothing() -> Option<Self> {
        Some(Self::default())
    }

    fn keywords(words: &[&'static str]) -> Option<Self> {
        Some(Self {
            keywords: words.to_vec(),
            ..Self::default()
        })
    }

    fn objects(classes: &[Class]) -> Option<Self> {
        Some(Self {
            classes: classes.to_vec(),
            schemas: true,
            databases: true,
            ..Self::default()
        })
    }

    fn tables() -> Option<Self> {
        Self::objects(&[Class::Table])
    }

    fn types(extra: &[&'static str]) -> Option<Self> {
        Some(Self {
            keywords: extra.to_vec(),
            classes: vec![Class::Type],
            types: true,
            ..Self::default()
        })
    }

    /// An expression over `columns` — none for a `DEFAULT`, which cannot
    /// name one.
    fn expression(columns: Vec<String>) -> Option<Self> {
        Some(Self {
            columns,
            expression: true,
            ..Self::default()
        })
    }

    fn columns(columns: Vec<String>) -> Option<Self> {
        Some(Self {
            columns,
            ..Self::default()
        })
    }
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

fn word<'t>(t: &'t [Tok<'_>], i: usize) -> &'t str {
    match t.get(i) {
        Some(tok) if tok.kind == ScanTokenKind::Word => tok.up.as_str(),
        _ => "",
    }
}

fn last_word<'t>(t: &'t [Tok<'_>]) -> &'t str {
    match t.len() {
        0 => "",
        n => word(t, n - 1),
    }
}

/// The parenthesis depth just past the last token of `t` (`base` when empty).
fn end_depth(t: &[Tok<'_>], base: usize) -> usize {
    match t.last() {
        Some(tok) if tok.kind == ScanTokenKind::LeftParen => tok.depth + 1,
        Some(tok) => tok.depth,
        None => base,
    }
}

/// The index just past the dotted name starting at `i` (`i` when no name
/// starts there).
fn name_end(t: &[Tok<'_>], i: usize) -> usize {
    if word(t, i).is_empty() {
        return i;
    }
    let mut end = i + 1;
    while t.get(end).is_some_and(|tok| tok.kind == ScanTokenKind::Dot)
        && !word(t, end + 1).is_empty()
    {
        end += 2;
    }
    end
}

fn name_text(t: &[Tok<'_>]) -> String {
    t.iter().map(|tok| tok.text).collect()
}

/// The tokens after the last comma at `depth` (all of `t` without one).
fn after_last_comma<'t, 'a>(t: &'t [Tok<'a>], depth: usize) -> &'t [Tok<'a>] {
    match t
        .iter()
        .rposition(|tok| tok.kind == ScanTokenKind::Comma && tok.depth == depth)
    {
        Some(i) => &t[i + 1..],
        None => t,
    }
}

/// The words of `t` that sit at `depth`.
fn words_at<'t>(t: &'t [Tok<'_>], depth: usize) -> Vec<&'t str> {
    t.iter()
        .filter(|tok| tok.kind == ScanTokenKind::Word && tok.depth == depth)
        .map(|tok| tok.up.as_str())
        .collect()
}

fn skip_if_exists(t: &[Tok<'_>], mut i: usize) -> usize {
    if word(t, i) == "IF" {
        i += 1;
        if word(t, i) == "NOT" {
            i += 1;
        }
        if word(t, i) == "EXISTS" {
            i += 1;
        }
    }
    i
}

// ---------------------------------------------------------------------------
// Statement heads
// ---------------------------------------------------------------------------

fn analyze(cx: &Cx<'_>, t: &[Tok<'_>], nest: u8) -> Option<Offer> {
    let head = t.first()?;
    if head.kind != ScanTokenKind::Word {
        return None;
    }
    let base = head.depth;
    if t.iter().any(|tok| tok.depth < base) {
        // The parenthesis the head sits in has closed.
        return None;
    }
    match head.up.as_str() {
        "CREATE" => create(cx, t, base, nest),
        "ALTER" => alter(cx, t, base, nest),
        "DROP" => drop_statement(cx, t, base),
        "TRUNCATE" => truncate(cx, t),
        "DECLARE" => declare(cx, t, base),
        "IF" | "BEGIN" | "ELSE" | "WHILE" => body(cx, &t[1..], nest),
        _ => None,
    }
}

/// The DDL statement the caret is in within a routine body or a control-flow
/// block, where statements are not separated for us: the last DDL head with
/// no other statement after it.
fn body(cx: &Cx<'_>, t: &[Tok<'_>], nest: u8) -> Option<Offer> {
    if nest >= 3 {
        return None;
    }
    let mut head: Option<usize> = None;
    let head_is = |head: Option<usize>, what: &str| head.is_some_and(|h| t[h].up == what);
    for (i, tok) in t.iter().enumerate() {
        if tok.kind != ScanTokenKind::Word {
            continue;
        }
        let previous = if i == 0 { "" } else { word(t, i - 1) };
        match tok.up.as_str() {
            "CREATE" | "TRUNCATE" | "DECLARE" => head = Some(i),
            // `ALTER TABLE t ALTER COLUMN …` / `… DROP COLUMN …` continue
            // the ALTER they belong to.
            "ALTER" if head_is(head, "ALTER") && word(t, i + 1) == "COLUMN" => {}
            "ALTER" => head = Some(i),
            "DROP" if head_is(head, "ALTER") => {}
            "DROP" => head = Some(i),
            // Referential actions (`ON DELETE SET NULL`) and Postgres'
            // `ALTER COLUMN c SET …` are part of the DDL statement.
            "DELETE" | "UPDATE" if previous == "ON" => {}
            "SET" if matches!(previous, "DELETE" | "UPDATE" | "COLUMN") => {}
            "SET" if head_is(head, "ALTER") => {}
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "SET" | "EXEC" | "EXECUTE"
            | "CALL" | "IF" | "ELSE" | "WHILE" | "BEGIN" | "END" | "RETURN" | "PRINT"
            | "RAISERROR" | "THROW" | "COMMIT" | "ROLLBACK" | "OPEN" | "FETCH" | "CLOSE"
            | "DEALLOCATE" | "USE" | "GO" | "THEN" | "LOOP" | "WITH"
                if !head_is(head, "CREATE") && !(tok.up == "WITH" && head.is_some()) =>
            {
                head = None;
            }
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "EXEC" | "EXECUTE" | "CALL"
            | "PRINT" | "RAISERROR" | "THROW" | "COMMIT" | "ROLLBACK"
                if head.is_some_and(|h| tok.depth <= t[h].depth)
                    && !create_head_takes(t, head, &tok.up) =>
            {
                head = None;
            }
            _ => {}
        }
    }
    analyze(cx, &t[head?..], nest + 1)
}

/// Whether the `CREATE` at `head` is one whose own head carries `word`
/// (a trigger's event list, a routine's `EXECUTE AS`).
fn create_head_takes(t: &[Tok<'_>], head: Option<usize>, word_up: &str) -> bool {
    let Some(head) = head else {
        return false;
    };
    if t[head].up != "CREATE" {
        return false;
    }
    let kinds = words_at(&t[head..], t[head].depth);
    let is_trigger = kinds.iter().take(6).any(|w| *w == "TRIGGER");
    let is_routine = kinds
        .iter()
        .take(6)
        .any(|w| matches!(*w, "PROCEDURE" | "PROC" | "FUNCTION"));
    match word_up {
        "INSERT" | "UPDATE" | "DELETE" => is_trigger,
        "EXECUTE" | "EXEC" => is_trigger || is_routine,
        _ => false,
    }
}

const CREATE_MODIFIERS: &[&str] = &[
    "UNIQUE",
    "CLUSTERED",
    "NONCLUSTERED",
    "COLUMNSTORE",
    "FULLTEXT",
    "SPATIAL",
    "TEMP",
    "TEMPORARY",
    "GLOBAL",
    "LOCAL",
    "UNLOGGED",
    "MATERIALIZED",
    "EXTERNAL",
    "VIRTUAL",
    "RECURSIVE",
    "EDITIONABLE",
    "NONEDITIONABLE",
];

/// The object kinds this module follows, as their head keyword.
const OBJECT_KINDS: &[&str] = &[
    "TABLE",
    "VIEW",
    "PROCEDURE",
    "PROC",
    "FUNCTION",
    "INDEX",
    "TRIGGER",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "TYPE",
    "SYNONYM",
];

/// The index of the object-kind keyword of a `CREATE` / `ALTER` / `DROP`
/// head, searching from `from` past any modifiers.
fn kind_index(t: &[Tok<'_>], from: usize, base: usize) -> Option<usize> {
    (from..t.len().min(from + 12)).find(|&i| {
        t[i].depth == base && t[i].kind == ScanTokenKind::Word && OBJECT_KINDS.contains(&word(t, i))
    })
}

fn create(cx: &Cx<'_>, t: &[Tok<'_>], base: usize, nest: u8) -> Option<Offer> {
    let mut i = 1;
    let mut existing = false;
    if word(t, i) == "OR" && matches!(word(t, i + 1), "ALTER" | "REPLACE") {
        existing = true;
        i += 2;
    }
    let Some(k) = kind_index(t, i, base) else {
        return create_kind_keywords(cx, &t[i..], existing);
    };
    object_head(cx, t, k, base, existing, nest)
}

/// `CREATE ▏` (with any modifiers typed so far): the kinds that can follow.
fn create_kind_keywords(cx: &Cx<'_>, modifiers: &[Tok<'_>], or_alter: bool) -> Option<Offer> {
    let words: Vec<&str> = modifiers.iter().map(|tok| tok.up.as_str()).collect();
    if modifiers.iter().any(|tok| tok.kind != ScanTokenKind::Word)
        || words
            .iter()
            .any(|w| !CREATE_MODIFIERS.contains(w) && *w != "OR")
    {
        return None;
    }
    if or_alter {
        return Offer::keywords(&["PROCEDURE", "FUNCTION", "VIEW", "TRIGGER"]);
    }
    if words == ["OR"] {
        return match cx.dialect {
            Dialect::SqlServer => Offer::keywords(&["ALTER"]),
            _ => Offer::keywords(&["REPLACE"]),
        };
    }
    if words
        .iter()
        .any(|w| matches!(*w, "UNIQUE" | "CLUSTERED" | "NONCLUSTERED" | "COLUMNSTORE"))
    {
        return match (cx.tsql(), words.last().copied()) {
            (true, Some("UNIQUE")) => {
                Offer::keywords(&["INDEX", "CLUSTERED INDEX", "NONCLUSTERED INDEX"])
            }
            _ => Offer::keywords(&["INDEX"]),
        };
    }
    if !words.is_empty() {
        return match words.last().copied() {
            Some("MATERIALIZED") => Offer::keywords(&["VIEW"]),
            Some("TEMP" | "TEMPORARY" | "UNLOGGED" | "EXTERNAL" | "VIRTUAL") => {
                Offer::keywords(&["TABLE"])
            }
            _ => None,
        };
    }
    let mut keywords = vec![
        "TABLE",
        "VIEW",
        "PROCEDURE",
        "FUNCTION",
        "INDEX",
        "UNIQUE INDEX",
        "TRIGGER",
        "SCHEMA",
        "DATABASE",
        "SEQUENCE",
        "TYPE",
    ];
    match cx.dialect {
        Dialect::SqlServer => keywords.extend(["OR ALTER", "SYNONYM"]),
        Dialect::Sqlite => {
            keywords.retain(|k| {
                !matches!(
                    *k,
                    "PROCEDURE" | "FUNCTION" | "SCHEMA" | "DATABASE" | "SEQUENCE" | "TYPE"
                )
            });
            keywords.push("TEMPORARY TABLE");
        }
        _ => keywords.extend(["OR REPLACE", "TEMPORARY TABLE"]),
    }
    Some(Offer {
        keywords,
        ..Offer::default()
    })
}

fn kind_keywords(cx: &Cx<'_>) -> Option<Offer> {
    let mut keywords = vec![
        "TABLE",
        "VIEW",
        "PROCEDURE",
        "FUNCTION",
        "INDEX",
        "TRIGGER",
        "SCHEMA",
        "DATABASE",
        "SEQUENCE",
        "TYPE",
    ];
    if cx.dialect == Dialect::Sqlite {
        keywords.truncate(2);
        keywords.extend(["INDEX", "TRIGGER"]);
    }
    Some(Offer {
        keywords,
        ..Offer::default()
    })
}

/// The classes of existing object a head keyword names.
fn kind_classes(kind: &str) -> &'static [Class] {
    match kind {
        "TABLE" => &[Class::Table],
        "VIEW" => &[Class::View],
        "PROCEDURE" | "PROC" => &[Class::Procedure],
        "FUNCTION" => &[Class::Function],
        "TYPE" => &[Class::Type],
        _ => &[],
    }
}

/// The name slot of a head: the existing objects of the kind when the
/// statement changes one, the schemas a new one can be created in otherwise.
fn name_slot(cx: &Cx<'_>, kind: &str, existing: bool) -> Option<Offer> {
    match kind {
        "SCHEMA" if existing => Some(Offer {
            schemas: true,
            ..Offer::default()
        }),
        "DATABASE" if existing => Some(Offer {
            databases: true,
            ..Offer::default()
        }),
        "SCHEMA" | "DATABASE" | "INDEX" => Offer::nothing(),
        _ if existing => Some(Offer {
            classes: kind_classes(kind).to_vec(),
            schemas: true,
            databases: kind == "TABLE",
            script_tables: false,
            ..Offer::default()
        }),
        // A new name: only the schema it goes in can be completed.
        _ => Some(Offer {
            schemas: !cx.prefix.contains('.'),
            ..Offer::default()
        }),
    }
}

/// A `CREATE` / `ALTER` head from its object-kind keyword at `k` on.
fn object_head(
    cx: &Cx<'_>,
    t: &[Tok<'_>],
    k: usize,
    base: usize,
    existing: bool,
    nest: u8,
) -> Option<Offer> {
    let kind = word(t, k);
    let mut n = skip_if_exists(t, k + 1);
    if kind == "INDEX" && word(t, n) == "CONCURRENTLY" {
        n = skip_if_exists(t, n + 1);
    }
    if n == t.len() {
        return name_slot(cx, kind, existing);
    }
    // Postgres lets an index go unnamed (`CREATE INDEX ON t (…)`).
    let after = if kind == "INDEX" && word(t, n) == "ON" {
        n
    } else {
        name_end(t, n)
    };
    if after == n && !(kind == "INDEX" && word(t, n) == "ON") {
        return None;
    }
    let name = name_text(&t[n..after]);
    let rest = &t[after..];
    match kind {
        "TABLE" if existing => alter_table(cx, &name, rest, base),
        "TABLE" => create_table(cx, rest, base),
        "VIEW" => view_head(cx, rest, base),
        "PROCEDURE" | "PROC" => routine_head(cx, rest, base, false, nest),
        "FUNCTION" => routine_head(cx, rest, base, true, nest),
        "INDEX" => index_head(cx, rest, base, existing),
        "TRIGGER" => trigger_head(cx, rest, base, nest),
        "TYPE" => type_head(cx, rest, base),
        "SEQUENCE" if !existing => sequence_head(cx, rest),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnMode {
    /// A definition inside `CREATE TABLE (…)`.
    Create,
    /// A definition after `ALTER TABLE t ADD`.
    Add,
    /// A redefinition of an existing column (`MODIFY COLUMN`).
    Redefine,
}

fn create_table(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize) -> Option<Offer> {
    let Some(open) = rest.first() else {
        // `CREATE TABLE t ▏`: the column list's parenthesis comes next.
        return Offer::nothing();
    };
    if open.kind != ScanTokenKind::LeftParen {
        // `CREATE TABLE t AS SELECT …` and table options.
        return None;
    }
    column_list(cx, &rest[1..], base + 1, ColumnMode::Create)
}

/// A parenthesised list of column and constraint definitions whose items
/// sit at `depth`; `inner` starts just past the opening parenthesis.
fn column_list(cx: &Cx<'_>, inner: &[Tok<'_>], depth: usize, mode: ColumnMode) -> Option<Offer> {
    if end_depth(inner, depth) < depth {
        // The list is closed: table options and whatever follows.
        return None;
    }
    let segment = after_last_comma(inner, depth);
    let own_columns = || defined_columns(inner, depth);
    column_definition(cx, segment, depth, mode, &own_columns)
}

/// The names of the columns a definition list declares.
fn defined_columns(inner: &[Tok<'_>], depth: usize) -> Vec<String> {
    let mut columns = Vec::new();
    let mut at_start = true;
    for tok in inner {
        if tok.depth != depth {
            continue;
        }
        match tok.kind {
            ScanTokenKind::Comma => at_start = true,
            ScanTokenKind::Word if at_start => {
                at_start = false;
                if !TABLE_CONSTRAINT_HEADS.contains(&tok.up.as_str()) {
                    columns.push(tok.text.to_string());
                }
            }
            _ => at_start = false,
        }
    }
    columns
}

const TABLE_CONSTRAINT_HEADS: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "FOREIGN",
    "UNIQUE",
    "CHECK",
    "INDEX",
    "KEY",
    "FULLTEXT",
    "SPATIAL",
    "EXCLUDE",
    "PERIOD",
    "LIKE",
];

const REFERENTIAL_ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];

/// The functions a column `DEFAULT` usually calls, most likely first.
fn usual_defaults(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => &[
            "SYSUTCDATETIME",
            "SYSDATETIME",
            "GETUTCDATE",
            "GETDATE",
            "NEWID",
            "NEWSEQUENTIALID",
            "SUSER_SNAME",
        ],
        Dialect::Postgres => &[
            "now",
            "CURRENT_TIMESTAMP",
            "gen_random_uuid",
            "nextval",
            "CURRENT_USER",
        ],
        Dialect::MySql => &["CURRENT_TIMESTAMP", "NOW", "UUID", "CURRENT_USER"],
        _ => &["CURRENT_TIMESTAMP", "CURRENT_DATE"],
    }
}

/// The words inside a definition that start an expression or a query; a
/// parenthesis after one of them is not a type's argument list.
const EXPRESSION_WORDS: &[&str] = &[
    "DEFAULT",
    "CHECK",
    "AS",
    "GENERATED",
    "VALUES",
    "WHERE",
    "WITH",
    "USING",
    "ON",
];

/// One column or constraint definition: `segment` holds its tokens so far,
/// its own words at `depth`. `table_columns` lists the columns of the table
/// being defined or altered.
fn column_definition(
    cx: &Cx<'_>,
    segment: &[Tok<'_>],
    depth: usize,
    mode: ColumnMode,
    table_columns: &dyn Fn() -> Vec<String>,
) -> Option<Offer> {
    let end = end_depth(segment, depth);
    if end > depth {
        return nested_in_definition(cx, segment, depth, end, table_columns);
    }
    if segment
        .last()
        .is_some_and(|tok| tok.kind == ScanTokenKind::Other)
    {
        // An operator: an expression is being written.
        return None;
    }
    let all_words = words_at(segment, depth);
    let mut words = all_words.as_slice();
    if words.first() == Some(&"CONSTRAINT") {
        match words.len() {
            1 => return Offer::nothing(),
            2 => {
                return Offer::keywords(&[
                    "PRIMARY KEY",
                    "FOREIGN KEY",
                    "UNIQUE",
                    "CHECK",
                    "DEFAULT",
                ]);
            }
            _ => words = &words[2..],
        }
    }
    let last = last_word(segment);
    let closed = segment
        .last()
        .is_some_and(|tok| tok.kind == ScanTokenKind::RightParen);
    let Some(first) = words.first().copied() else {
        return match mode {
            ColumnMode::Redefine => Offer::columns(table_columns()),
            ColumnMode::Create => Offer::keywords(&[
                "CONSTRAINT",
                "PRIMARY KEY",
                "FOREIGN KEY",
                "UNIQUE",
                "CHECK",
            ]),
            ColumnMode::Add if cx.tsql() => Offer::keywords(&[
                "CONSTRAINT",
                "PRIMARY KEY",
                "FOREIGN KEY",
                "UNIQUE",
                "CHECK",
            ]),
            ColumnMode::Add => Offer::keywords(&[
                "COLUMN",
                "CONSTRAINT",
                "PRIMARY KEY",
                "FOREIGN KEY",
                "UNIQUE",
                "CHECK",
            ]),
        };
    };
    // The statement could be complete here; on a fresh line the next one is
    // being started.
    let maybe_done = mode != ColumnMode::Create && cx.fresh_line;

    if TABLE_CONSTRAINT_HEADS.contains(&first) || first == "DEFAULT" {
        if words.iter().any(|w| matches!(*w, "CHECK" | "DEFAULT")) {
            return None;
        }
        if let Some(offer) = constraint_tail(cx, last, segment) {
            return Some(offer);
        }
        if maybe_done {
            return None;
        }
        return match last {
            "KEY" | "UNIQUE" if cx.tsql() => Offer::keywords(&["CLUSTERED", "NONCLUSTERED"]),
            _ if closed && first == "FOREIGN" && !words.contains(&"REFERENCES") => {
                Offer::keywords(&["REFERENCES"])
            }
            _ if closed && words.contains(&"REFERENCES") => {
                Offer::keywords(&["ON DELETE", "ON UPDATE"])
            }
            _ => Offer::nothing(),
        };
    }

    // A column: its name, then its type, then its constraints.
    if words.len() == 1 && !closed {
        return Offer::types(&[]);
    }
    if words.get(1) == Some(&"AS") {
        // A computed column's expression.
        return Offer::expression(table_columns());
    }
    if last == "DEFAULT" {
        // The default's expression is being written.
        return Offer::expression(Vec::new());
    }
    if let Some(offer) = constraint_tail(cx, last, segment) {
        return Some(offer);
    }
    match last {
        "NOT" => return Offer::keywords(&["NULL"]),
        "CONSTRAINT" | "COLLATE" | "CHECK" | "IDENTITY" => return Offer::nothing(),
        "GENERATED" => {
            return Offer::keywords(&["ALWAYS AS IDENTITY", "BY DEFAULT AS IDENTITY", "ALWAYS AS"]);
        }
        _ => {}
    }
    if maybe_done {
        return None;
    }
    let mut keywords = vec![
        "NOT NULL",
        "NULL",
        "PRIMARY KEY",
        "DEFAULT",
        "REFERENCES",
        "UNIQUE",
        "CHECK",
        "CONSTRAINT",
        "COLLATE",
    ];
    match cx.dialect {
        Dialect::SqlServer => keywords.push("IDENTITY"),
        Dialect::MySql => keywords.extend(["AUTO_INCREMENT", "COMMENT"]),
        Dialect::Postgres | Dialect::Oracle => keywords.push("GENERATED ALWAYS AS IDENTITY"),
        Dialect::Sqlite => keywords.push("AUTOINCREMENT"),
    }
    Some(Offer {
        keywords,
        ..Offer::default()
    })
}

/// The slots that follow a constraint keyword whichever definition it is in.
fn constraint_tail(cx: &Cx<'_>, last: &str, segment: &[Tok<'_>]) -> Option<Offer> {
    let previous = match segment.len() {
        0 | 1 => "",
        n => word(segment, n - 2),
    };
    match last {
        "PRIMARY" | "FOREIGN" => Offer::keywords(&["KEY"]),
        "REFERENCES" => Offer::tables(),
        "ON" => Offer::keywords(&["DELETE", "UPDATE"]),
        "DELETE" | "UPDATE" if previous == "ON" => {
            if cx.tsql() {
                Offer::keywords(REFERENTIAL_ACTIONS)
            } else {
                Offer::keywords(&[
                    "CASCADE",
                    "SET NULL",
                    "SET DEFAULT",
                    "NO ACTION",
                    "RESTRICT",
                ])
            }
        }
        "SET" if matches!(previous, "DELETE" | "UPDATE") => Offer::keywords(&["NULL", "DEFAULT"]),
        "NO" => Offer::keywords(&["ACTION"]),
        _ => None,
    }
}

/// The caret is inside a parenthesis within a definition: a key's column
/// list, a referenced table's columns, a type's arguments — or an
/// expression, which is not ours.
fn nested_in_definition(
    cx: &Cx<'_>,
    segment: &[Tok<'_>],
    depth: usize,
    end: usize,
    table_columns: &dyn Fn() -> Vec<String>,
) -> Option<Offer> {
    let open = segment
        .iter()
        .rposition(|tok| tok.kind == ScanTokenKind::LeftParen && tok.depth == depth)?;
    let before = words_at(&segment[..open], depth);
    let opener = last_word(&segment[..open]);
    let inner = &segment[open + 1..];
    let deeper = end > depth + 1;
    if matches!(
        opener,
        "KEY" | "UNIQUE" | "CLUSTERED" | "NONCLUSTERED" | "INDEX"
    ) {
        if deeper {
            return None;
        }
        return key_columns(inner, depth + 1, table_columns(), true);
    }
    if let Some(reference) = segment[..open]
        .iter()
        .rposition(|tok| tok.depth == depth && tok.up == "REFERENCES")
        && reference + 1 < open
        && name_end(segment, reference + 1) == open
    {
        if deeper {
            return None;
        }
        let table = name_text(&segment[reference + 1..open]);
        return key_columns(inner, depth + 1, columns_of(cx.catalog, &table), false);
    }
    if before.iter().any(|w| matches!(*w, "CHECK" | "AS")) {
        return Offer::expression(table_columns());
    }
    if before.contains(&"DEFAULT") {
        return Offer::expression(Vec::new());
    }
    if opener.is_empty() || before.iter().any(|w| EXPRESSION_WORDS.contains(w)) {
        return None;
    }
    if before.len() < 2 {
        // `name (`: not a column definition we know.
        return None;
    }
    type_arguments(cx, opener)
}

/// A type's argument list: sizes are typed, not completed — apart from
/// SQL Server's `MAX`.
fn type_arguments(cx: &Cx<'_>, type_word: &str) -> Option<Offer> {
    if cx.tsql() && matches!(type_word, "VARCHAR" | "NVARCHAR" | "VARBINARY") {
        Offer::keywords(&["MAX"])
    } else {
        Offer::nothing()
    }
}

/// A parenthesised column list: the columns at each item's start, then the
/// sort direction where the list takes one.
fn key_columns(
    inner: &[Tok<'_>],
    depth: usize,
    columns: Vec<String>,
    directions: bool,
) -> Option<Offer> {
    let item = after_last_comma(inner, depth);
    if item.is_empty() {
        let listed: Vec<&str> = inner
            .iter()
            .filter(|tok| tok.kind == ScanTokenKind::Word)
            .map(|tok| tok.up.as_str())
            .collect();
        return Offer::columns(
            columns
                .into_iter()
                .filter(|c| !listed.contains(&c.to_ascii_uppercase().as_str()))
                .collect(),
        );
    }
    if directions && item.len() == 1 {
        return Offer::keywords(&["ASC", "DESC"]);
    }
    Offer::nothing()
}

/// The columns of the catalog table or view called `name` (optionally
/// schema-qualified).
fn columns_of(catalog: &CompletionCatalog, name: &str) -> Vec<String> {
    let mut parts = name.rsplit('.');
    let table = parts.next().unwrap_or_default();
    let schema = parts.next();
    for object in &catalog.objects {
        if matches!(object.kind, ObjectKind::Table | ObjectKind::View)
            && object.name.eq_ignore_ascii_case(table)
            && schema.is_none_or(|s| {
                object
                    .schema
                    .as_deref()
                    .is_some_and(|os| os.eq_ignore_ascii_case(s))
            })
        {
            return object.columns.clone();
        }
    }
    super::filesystem::find_filesystem_table_columns(catalog, name)
        .into_iter()
        .map(|(column, _)| column.to_string())
        .collect()
}

fn alter_table(cx: &Cx<'_>, table: &str, rest: &[Tok<'_>], base: usize) -> Option<Offer> {
    let columns = || columns_of(cx.catalog, table);
    if rest.is_empty() {
        if cx.fresh_line {
            return None;
        }
        return match cx.dialect {
            Dialect::SqlServer => Offer::keywords(&[
                "ADD",
                "ALTER COLUMN",
                "DROP COLUMN",
                "DROP CONSTRAINT",
                "ADD CONSTRAINT",
                "WITH CHECK",
                "CHECK CONSTRAINT",
                "NOCHECK CONSTRAINT",
                "ENABLE TRIGGER",
                "DISABLE TRIGGER",
                "REBUILD",
            ]),
            Dialect::MySql => Offer::keywords(&[
                "ADD COLUMN",
                "MODIFY COLUMN",
                "CHANGE COLUMN",
                "DROP COLUMN",
                "ADD CONSTRAINT",
                "DROP CONSTRAINT",
                "ADD INDEX",
                "DROP INDEX",
                "RENAME COLUMN",
                "RENAME TO",
            ]),
            Dialect::Oracle => Offer::keywords(&[
                "ADD",
                "MODIFY",
                "DROP COLUMN",
                "ADD CONSTRAINT",
                "DROP CONSTRAINT",
                "RENAME COLUMN",
                "RENAME TO",
            ]),
            Dialect::Postgres => Offer::keywords(&[
                "ADD COLUMN",
                "ALTER COLUMN",
                "DROP COLUMN",
                "ADD CONSTRAINT",
                "DROP CONSTRAINT",
                "RENAME COLUMN",
                "RENAME TO",
                "OWNER TO",
            ]),
            Dialect::Sqlite => {
                Offer::keywords(&["ADD COLUMN", "DROP COLUMN", "RENAME COLUMN", "RENAME TO"])
            }
        };
    }
    // The action the caret is in: the last one opened at the statement's
    // own depth.
    let action = rest.iter().rposition(|tok| {
        tok.depth == base
            && matches!(
                tok.up.as_str(),
                "ADD" | "DROP" | "ALTER" | "MODIFY" | "CHANGE" | "RENAME"
            )
    })?;
    // `ON DELETE …`/`SET DEFAULT` inside an ADD belong to it; only a word
    // that starts a clause counts.
    let action = first_action_of_clause(rest, action, base);
    let verb = word(rest, action);
    let mut tail = &rest[action + 1..];
    match verb {
        "ADD" => {
            if word(tail, 0) == "COLUMN" {
                tail = &tail[skip_if_exists(tail, 1)..];
                if tail.is_empty() {
                    return Offer::nothing();
                }
            }
            let segment = after_last_comma(tail, base);
            column_definition(cx, segment, base, ColumnMode::Add, &columns)
        }
        "DROP" => match word(tail, 0) {
            "" if tail.is_empty() => {
                let mut offer = Offer::keywords(&["COLUMN", "CONSTRAINT"])?;
                if !cx.tsql() {
                    offer.columns = columns();
                }
                Some(offer)
            }
            "COLUMN" => {
                let listed = &tail[skip_if_exists(tail, 1)..];
                if after_last_comma(listed, base).is_empty() {
                    Offer::columns(columns())
                } else if cx.fresh_line {
                    None
                } else {
                    Offer::nothing()
                }
            }
            "CONSTRAINT" | "INDEX" | "KEY" if tail.len() == 1 => Offer::nothing(),
            _ => None,
        },
        "ALTER" => {
            let named = match word(tail, 0) {
                "COLUMN" => &tail[1..],
                _ if cx.tsql() => return Offer::keywords(&["COLUMN"]).filter(|_| tail.is_empty()),
                _ => tail,
            };
            match named.len() {
                0 => Offer::columns(columns()),
                1 if cx.tsql() => Offer::types(&[]),
                1 if cx.dialect == Dialect::MySql => {
                    Offer::keywords(&["SET DEFAULT", "DROP DEFAULT"])
                }
                1 => Offer::keywords(&[
                    "TYPE",
                    "SET DATA TYPE",
                    "SET DEFAULT",
                    "DROP DEFAULT",
                    "SET NOT NULL",
                    "DROP NOT NULL",
                ]),
                _ if last_word(named) == "TYPE" => Offer::types(&[]),
                _ if cx.tsql() && end_depth(named, base) > base => {
                    type_arguments(cx, word(named, 1))
                }
                _ if cx.tsql() && !cx.fresh_line && !named.iter().any(is_operator) => {
                    match last_word(named) {
                        "NOT" => Offer::keywords(&["NULL"]),
                        "COLLATE" => Offer::nothing(),
                        _ => Offer::keywords(&["NULL", "NOT NULL", "COLLATE"]),
                    }
                }
                _ => None,
            }
        }
        "MODIFY" => {
            if word(tail, 0) == "COLUMN" {
                tail = &tail[1..];
            }
            column_definition(cx, tail, base, ColumnMode::Redefine, &columns)
        }
        "CHANGE" => {
            if word(tail, 0) == "COLUMN" {
                tail = &tail[1..];
            }
            match tail.len() {
                0 => Offer::columns(columns()),
                1 => Offer::nothing(),
                _ => column_definition(cx, &tail[1..], base, ColumnMode::Add, &columns),
            }
        }
        "RENAME" => match (word(tail, 0), tail.len()) {
            ("", 0) => Offer::keywords(&["COLUMN", "TO", "CONSTRAINT"]),
            ("COLUMN", 1) => Offer::columns(columns()),
            ("COLUMN", 2) => Offer::keywords(&["TO"]),
            _ => Offer::nothing(),
        },
        _ => None,
    }
}

fn is_operator(tok: &Tok<'_>) -> bool {
    tok.kind == ScanTokenKind::Other
}

/// `ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES t (b) ON DELETE SET
/// DEFAULT`, `ADD c int … ON UPDATE …`: an action word that follows `ON` or
/// `SET` continues the clause before it, so step back to that clause's own
/// action word.
fn first_action_of_clause(rest: &[Tok<'_>], mut action: usize, base: usize) -> usize {
    loop {
        let previous = if action == 0 {
            ""
        } else {
            word(rest, action - 1)
        };
        let continues = matches!(previous, "ON" | "SET")
            || (rest[action].up == "DROP" && previous == "ALTER")
            || (matches!(rest[action].up.as_str(), "ADD" | "DROP") && previous == "COLUMN");
        if !continues {
            return action;
        }
        let Some(earlier) = rest[..action].iter().rposition(|tok| {
            tok.depth == base
                && matches!(
                    tok.up.as_str(),
                    "ADD" | "DROP" | "ALTER" | "MODIFY" | "CHANGE" | "RENAME"
                )
        }) else {
            return action;
        };
        action = earlier;
    }
}

// ---------------------------------------------------------------------------
// Views, indexes, triggers, types
// ---------------------------------------------------------------------------

fn view_head(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize) -> Option<Offer> {
    if rest
        .iter()
        .any(|tok| tok.depth == base && tok.kind == ScanTokenKind::Word && tok.up == "AS")
    {
        // The view's query, which `AS` has just opened or which is under way.
        return if last_word(rest) == "AS" && end_depth(rest, base) == base {
            Offer::keywords(&["SELECT", "WITH"])
        } else {
            None
        };
    }
    if end_depth(rest, base) > base {
        // The view's own column names.
        return Offer::nothing();
    }
    match last_word(rest) {
        "WITH" if cx.tsql() => Offer::keywords(&["SCHEMABINDING", "ENCRYPTION", "VIEW_METADATA"]),
        "WITH" => None,
        _ if cx.tsql() => Offer::keywords(&["AS", "WITH SCHEMABINDING"]),
        _ => Offer::keywords(&["AS"]),
    }
}

fn index_head(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize, existing: bool) -> Option<Offer> {
    let Some(on) = rest
        .iter()
        .position(|tok| tok.depth == base && tok.up == "ON")
    else {
        return if rest.is_empty() {
            Offer::keywords(&["ON"])
        } else {
            None
        };
    };
    if on != 0 {
        return None;
    }
    let mut start = 1;
    if word(rest, start) == "ONLY" {
        start += 1;
    }
    if start == rest.len() {
        return Offer::tables();
    }
    let after = name_end(rest, start);
    if after == start {
        return None;
    }
    let table = name_text(&rest[start..after]);
    let mut tail = &rest[after..];
    if existing {
        // `ALTER INDEX ix ON t ▏`
        return if tail.is_empty() && cx.tsql() && !cx.fresh_line {
            Offer::keywords(&["REBUILD", "REORGANIZE", "DISABLE"])
        } else {
            None
        };
    }
    if word(tail, 0) == "USING" {
        if tail.len() < 2 {
            return match cx.dialect {
                Dialect::Postgres => Offer::keywords(&["BTREE", "HASH", "GIN", "GIST", "BRIN"]),
                _ => Offer::keywords(&["BTREE", "HASH"]),
            };
        }
        tail = &tail[2..];
    }
    let Some(open) = tail.first() else {
        return match cx.dialect {
            Dialect::Postgres => Offer::keywords(&["USING"]),
            _ => Offer::nothing(),
        };
    };
    if open.kind != ScanTokenKind::LeftParen {
        return None;
    }
    let end = end_depth(tail, base);
    let columns = || columns_of(cx.catalog, &table);
    // The key list is the first parenthesis; `INCLUDE (…)` may follow it.
    let closing = tail
        .iter()
        .position(|tok| tok.kind == ScanTokenKind::RightParen && tok.depth == base);
    let Some(closing) = closing else {
        if end > base + 1 {
            // An expression key.
            return None;
        }
        return key_columns(&tail[1..], base + 1, columns(), true);
    };
    let after_keys = &tail[closing + 1..];
    if end > base {
        let open = after_keys
            .iter()
            .rposition(|tok| tok.kind == ScanTokenKind::LeftParen && tok.depth == base)?;
        if last_word(&after_keys[..open]) == "INCLUDE" && end == base + 1 {
            return key_columns(&after_keys[open + 1..], base + 1, columns(), false);
        }
        return None;
    }
    match last_word(after_keys) {
        "INCLUDE" => Offer::nothing(),
        "WHERE" | "AND" | "OR" => Offer::columns(columns()),
        _ if after_keys.iter().any(|tok| tok.up == "WHERE") => None,
        _ if cx.fresh_line => None,
        _ => match cx.dialect {
            Dialect::SqlServer | Dialect::Postgres => {
                let all = ["INCLUDE", "WHERE", "WITH"];
                let unused: Vec<&'static str> = all
                    .into_iter()
                    .filter(|k| !after_keys.iter().any(|tok| tok.up == *k))
                    .collect();
                Some(Offer {
                    keywords: unused,
                    ..Offer::default()
                })
            }
            Dialect::Sqlite => Offer::keywords(&["WHERE"]),
            _ => Offer::nothing(),
        },
    }
}

fn trigger_head(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize, nest: u8) -> Option<Offer> {
    // SQL Server's body follows `AS`; the other engines' follows the head
    // without a marker word of its own.
    if cx.tsql() {
        if let Some(body_at) = rest
            .iter()
            .position(|tok| tok.depth == base && tok.kind == ScanTokenKind::Word && tok.up == "AS")
        {
            return body(cx, &rest[body_at + 1..], nest);
        }
    } else if rest.iter().any(|tok| {
        tok.kind == ScanTokenKind::Word && matches!(tok.up.as_str(), "BEGIN" | "DECLARE" | "WHEN")
    }) {
        return None;
    }
    if end_depth(rest, base) > base {
        return None;
    }
    let last = last_word(rest);
    let previous = match rest.len() {
        0 | 1 => "",
        n => word(rest, n - 2),
    };
    let events = Offer::keywords(&["INSERT", "UPDATE", "DELETE"]);
    if rest
        .last()
        .is_some_and(|tok| tok.kind == ScanTokenKind::Comma)
    {
        return events;
    }
    // A table name after `ON`.
    let on = rest.iter().rposition(|tok| tok.up == "ON");
    let after_table =
        on.is_some_and(|on| on + 1 < rest.len() && name_end(rest, on + 1) == rest.len());
    match last {
        "" if rest.is_empty() => {
            if cx.tsql() {
                Offer::keywords(&["ON"])
            } else {
                match cx.dialect {
                    Dialect::MySql => Offer::keywords(&["BEFORE", "AFTER"]),
                    _ => Offer::keywords(&["BEFORE", "AFTER", "INSTEAD OF"]),
                }
            }
        }
        "ON" => Offer::objects(&[Class::Table, Class::View]),
        _ if after_table && cx.tsql() => Offer::keywords(&["AFTER", "FOR", "INSTEAD OF"]),
        _ if after_table => match cx.dialect {
            Dialect::MySql => Offer::keywords(&["FOR EACH ROW"]),
            Dialect::Postgres => {
                Offer::keywords(&["FOR EACH ROW", "FOR EACH STATEMENT", "EXECUTE FUNCTION"])
            }
            _ => Offer::keywords(&["FOR EACH ROW", "BEGIN"]),
        },
        "INSTEAD" => Offer::keywords(&["OF"]),
        "BEFORE" | "AFTER" | "OF" | "OR" => events,
        "FOR" if cx.tsql() => events,
        "FOR" => Offer::keywords(&["EACH ROW"]),
        "EACH" => Offer::keywords(&["ROW"]),
        "INSERT" | "UPDATE" | "DELETE" if cx.tsql() => Offer::keywords(&["AS"]),
        "INSERT" | "UPDATE" | "DELETE" => Offer::keywords(&["ON"]),
        "FUNCTION" | "PROCEDURE" if previous == "EXECUTE" => Offer::objects(&[Class::Function]),
        "EXECUTE" => Offer::keywords(&["FUNCTION"]),
        "ROW" | "STATEMENT" if cx.dialect == Dialect::Postgres => {
            Offer::keywords(&["EXECUTE FUNCTION", "WHEN"])
        }
        _ => None,
    }
}

fn sequence_head(cx: &Cx<'_>, rest: &[Tok<'_>]) -> Option<Offer> {
    if rest.last().is_some_and(is_operator) {
        return None;
    }
    match last_word(rest) {
        "AS" => Offer::types(&[]),
        "WITH" | "BY" | "MINVALUE" | "MAXVALUE" | "CACHE" | "START" | "INCREMENT" => {
            Offer::nothing()
        }
        "NO" => Offer::keywords(&["MINVALUE", "MAXVALUE", "CYCLE", "CACHE"]),
        _ if cx.fresh_line && !rest.is_empty() => None,
        _ => {
            let all = [
                "AS",
                "START WITH",
                "INCREMENT BY",
                "MINVALUE",
                "MAXVALUE",
                "CYCLE",
                "CACHE",
            ];
            let used = |keyword: &str| {
                let head = keyword.split(' ').next().unwrap_or(keyword);
                rest.iter().any(|tok| tok.up == head)
            };
            Some(Offer {
                keywords: all.into_iter().filter(|k| !used(k)).collect(),
                ..Offer::default()
            })
        }
    }
}

fn type_head(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize) -> Option<Offer> {
    match (word(rest, 0), word(rest, 1)) {
        ("FROM", "") if rest.len() == 1 => Offer::types(&[]),
        ("AS", "TABLE")
            if rest
                .get(2)
                .is_some_and(|tok| tok.kind == ScanTokenKind::LeftParen) =>
        {
            column_list(cx, &rest[3..], base + 1, ColumnMode::Create)
        }
        ("", "") if rest.is_empty() && cx.tsql() => Offer::keywords(&["FROM", "AS TABLE"]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Routines
// ---------------------------------------------------------------------------

/// Parameter modes, which sit beside a parameter's name rather than being
/// its name or type.
const PARAMETER_MODES: &[&str] = &["IN", "OUT", "INOUT", "VARIADIC"];

fn routine_head(
    cx: &Cx<'_>,
    rest: &[Tok<'_>],
    base: usize,
    function: bool,
    nest: u8,
) -> Option<Offer> {
    if let Some(body_at) = routine_body_start(cx, rest, base) {
        return body(cx, &rest[body_at + 1..], nest);
    }
    let end = end_depth(rest, base);
    let parenthesised = rest
        .first()
        .is_some_and(|tok| tok.kind == ScanTokenKind::LeftParen);
    let closing = rest
        .iter()
        .position(|tok| tok.kind == ScanTokenKind::RightParen && tok.depth == base);

    // Past the parameter list (or, for a SQL Server procedure, alongside it).
    if let (true, Some(closing)) = (parenthesised, closing) {
        return after_parameters(cx, &rest[closing + 1..], base, function);
    }
    if parenthesised {
        let inner = &rest[1..];
        let segment = after_last_comma(inner, base + 1);
        return parameter(cx, segment, base + 1, end, function, false);
    }
    if !cx.tsql() {
        // Oracle routines may take no parameter list at all.
        return after_parameters(cx, rest, base, function);
    }
    if function {
        return if rest.is_empty() {
            Offer::nothing()
        } else {
            after_parameters(cx, rest, base, function)
        };
    }
    // A SQL Server procedure's bare parameter list, with its options after.
    if let Some(with) = rest
        .iter()
        .position(|tok| tok.depth == base && tok.up == "WITH")
    {
        return after_parameters(cx, &rest[with..], base, function);
    }
    let segment = after_last_comma(rest, base);
    parameter(cx, segment, base, end, function, true)
}

/// The index of the word that opens a routine's body, when the caret is
/// past it.
fn routine_body_start(cx: &Cx<'_>, rest: &[Tok<'_>], base: usize) -> Option<usize> {
    rest.iter().enumerate().position(|(i, tok)| {
        if tok.depth != base || tok.kind != ScanTokenKind::Word {
            return false;
        }
        let previous = if i == 0 { "" } else { word(rest, i - 1) };
        match tok.up.as_str() {
            // `@p AS int` and `EXECUTE AS OWNER` are part of the head.
            "AS" => !(previous.starts_with('@') || matches!(previous, "EXECUTE" | "EXEC")),
            "BEGIN" | "LANGUAGE" => true,
            "IS" => !cx.tsql(),
            // Oracle's function head has its own `RETURN type`.
            "RETURN" => cx.dialect != Dialect::Oracle,
            _ => false,
        }
    })
}

/// One parameter: its name, then its type, then (SQL Server) its options.
fn parameter(
    cx: &Cx<'_>,
    segment: &[Tok<'_>],
    depth: usize,
    end: usize,
    function: bool,
    bare_list: bool,
) -> Option<Offer> {
    if end > depth {
        // A type's arguments, unless a default's expression got there first.
        let words = words_at(segment, depth);
        if words.contains(&"DEFAULT") || segment.iter().any(is_operator) {
            return None;
        }
        let open = segment
            .iter()
            .rposition(|tok| tok.kind == ScanTokenKind::LeftParen && tok.depth == depth)?;
        return type_arguments(cx, last_word(&segment[..open]));
    }
    let words: Vec<&str> = words_at(segment, depth)
        .into_iter()
        .filter(|w| !PARAMETER_MODES.contains(w))
        .collect();
    if segment.last().is_some_and(is_operator) {
        // `@p int = ▏`: a default is a constant.
        return if cx.tsql() {
            Offer::keywords(&["NULL"])
        } else {
            None
        };
    }
    match words.as_slice() {
        [] => {
            if cx.prefix.starts_with('@') {
                return Offer::nothing();
            }
            if cx.tsql() && bare_list && segment.is_empty() && depth_is_first_parameter(cx) {
                return Offer::keywords(&["AS", "WITH RECOMPILE", "WITH EXECUTE AS"]);
            }
            if cx.tsql() {
                Offer::nothing()
            } else {
                Offer::keywords(&["IN", "OUT", "INOUT"])
                    .filter(|_| segment.is_empty())
                    .or_else(Offer::nothing)
            }
        }
        [_] | [_, "AS"] => Offer::types(&[]),
        _ if words.contains(&"DEFAULT") && last_word(segment) == "DEFAULT" => None,
        _ if cx.tsql() => {
            if segment.iter().any(is_operator) {
                // A default is already written.
                return if function {
                    Offer::nothing()
                } else if bare_list {
                    Offer::keywords(&["OUTPUT", "AS", "READONLY"])
                } else {
                    Offer::keywords(&["OUTPUT", "READONLY"])
                };
            }
            if function {
                Offer::keywords(&["READONLY"])
            } else if bare_list && cx.fresh_line {
                // A line of its own after the parameters opens the body.
                Offer::keywords(&["AS", "WITH RECOMPILE", "OUTPUT", "READONLY"])
            } else if bare_list {
                Offer::keywords(&["OUTPUT", "READONLY", "AS", "WITH RECOMPILE"])
            } else {
                Offer::keywords(&["OUTPUT", "READONLY"])
            }
        }
        _ => Offer::keywords(&["DEFAULT"]),
    }
}

/// Whether the caret follows the routine's name directly (no parameter
/// written yet), where `AS` may open the body.
fn depth_is_first_parameter(cx: &Cx<'_>) -> bool {
    !cx.sql[..cx.prefix_start].trim_end().ends_with(',')
}

/// After a routine's parameter list: the return type, then the options that
/// lead to the body.
fn after_parameters(cx: &Cx<'_>, tail: &[Tok<'_>], base: usize, function: bool) -> Option<Offer> {
    let end = end_depth(tail, base);
    let returns = tail
        .iter()
        .position(|tok| tok.depth == base && matches!(tok.up.as_str(), "RETURNS" | "RETURN"));
    if let Some(returns) = returns {
        let after = &tail[returns + 1..];
        if end > base {
            // `RETURNS @t TABLE (…)` / `RETURNS TABLE (…)` declare columns.
            let open = after
                .iter()
                .position(|tok| tok.kind == ScanTokenKind::LeftParen && tok.depth == base)?;
            if last_word(&after[..open]) == "TABLE" {
                return column_list(cx, &after[open + 1..], base + 1, ColumnMode::Create);
            }
            return if open == 1 && end == base + 1 {
                type_arguments(cx, word(after, 0))
            } else {
                None
            };
        }
        return match after.len() {
            0 => match cx.dialect {
                Dialect::SqlServer => Offer::types(&["TABLE"]),
                Dialect::Postgres => Offer::types(&["TABLE", "SETOF", "VOID", "TRIGGER"]),
                _ => Offer::types(&[]),
            },
            1 if after[0].up.starts_with('@') => Offer::keywords(&["TABLE"]),
            _ if last_word(after) == "WITH" && cx.tsql() => Offer::keywords(&[
                "SCHEMABINDING",
                "RETURNS NULL ON NULL INPUT",
                "EXECUTE AS",
                "INLINE",
            ]),
            _ => match cx.dialect {
                Dialect::SqlServer => Offer::keywords(&["AS", "WITH SCHEMABINDING", "BEGIN"]),
                Dialect::Postgres => Offer::keywords(&["LANGUAGE", "AS"]),
                Dialect::Oracle => Offer::keywords(&["IS", "AS"]),
                _ => None,
            },
        };
    }
    if end > base {
        return None;
    }
    if cx.tsql() && !function {
        return match last_word(tail) {
            "WITH" => Offer::keywords(&["RECOMPILE", "ENCRYPTION", "EXECUTE AS"]),
            "EXECUTE" | "EXEC" => Offer::keywords(&["AS"]),
            "AS" => Offer::keywords(&["OWNER", "CALLER", "SELF"]),
            _ => Offer::keywords(&["AS", "WITH RECOMPILE"]),
        };
    }
    if !tail.is_empty() {
        return None;
    }
    match (cx.dialect, function) {
        (Dialect::Oracle, true) => Offer::keywords(&["RETURN"]),
        (Dialect::Oracle, false) => Offer::keywords(&["IS", "AS"]),
        (Dialect::Postgres, false) => Offer::keywords(&["LANGUAGE", "AS"]),
        (Dialect::MySql, false) => None,
        (_, true) => Offer::keywords(&["RETURNS"]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ALTER / DROP / TRUNCATE / DECLARE
// ---------------------------------------------------------------------------

fn alter(cx: &Cx<'_>, t: &[Tok<'_>], base: usize, nest: u8) -> Option<Offer> {
    if t.len() == 1 {
        return kind_keywords(cx);
    }
    let k = kind_index(t, 1, base)?;
    if k != 1
        && !t[1..k]
            .iter()
            .all(|tok| CREATE_MODIFIERS.contains(&tok.up.as_str()))
    {
        return None;
    }
    let mut end = k;
    // `ALTER TABLE IF EXISTS ONLY t`
    if word(t, k) == "TABLE" {
        end = skip_if_exists(t, k + 1) - 1;
        if word(t, end + 1) == "ONLY" {
            end += 1;
        }
    }
    if end + 1 == t.len() {
        return name_slot(cx, word(t, k), true);
    }
    if end != k {
        // Re-read the head with the optional words out of the way.
        let after = name_end(t, end + 1);
        if after == end + 1 {
            return None;
        }
        return alter_table(cx, &name_text(&t[end + 1..after]), &t[after..], base);
    }
    object_head(cx, t, k, base, true, nest)
}

fn drop_statement(cx: &Cx<'_>, t: &[Tok<'_>], base: usize) -> Option<Offer> {
    if t.len() == 1 {
        return kind_keywords(cx);
    }
    let k = kind_index(t, 1, base)?;
    if k != 1
        && !t[1..k]
            .iter()
            .all(|tok| CREATE_MODIFIERS.contains(&tok.up.as_str()))
    {
        return None;
    }
    let kind = word(t, k);
    let mut names = &t[skip_if_exists(t, k + 1)..];
    if kind == "INDEX" && word(names, 0) == "CONCURRENTLY" {
        names = &names[skip_if_exists(names, 1)..];
    }
    let item = after_last_comma(names, base);
    if item.is_empty() {
        let mut offer = name_slot(cx, kind, true)?;
        offer.script_tables = kind == "TABLE";
        return Some(offer);
    }
    // Past a name: SQL Server's and MySQL's `DROP INDEX ix ON ▏table`.
    if kind == "INDEX" && last_word(item) == "ON" && name_end(item, 0) + 1 == item.len() {
        return Offer::tables();
    }
    if kind == "INDEX"
        && name_end(item, 0) == item.len()
        && !cx.fresh_line
        && matches!(cx.dialect, Dialect::SqlServer | Dialect::MySql)
    {
        return Offer::keywords(&["ON"]);
    }
    None
}

fn truncate(cx: &Cx<'_>, t: &[Tok<'_>]) -> Option<Offer> {
    let mut i = 1;
    if word(t, i) == "TABLE" {
        i += 1;
    } else if cx.tsql() || cx.dialect == Dialect::Oracle {
        return if t.len() == 1 {
            Offer::keywords(&["TABLE"])
        } else {
            None
        };
    }
    if word(t, i) == "ONLY" {
        i += 1;
    }
    if i == t.len() {
        let mut offer = Offer::tables()?;
        if t.len() == 1 {
            offer.keywords.push("TABLE");
        }
        return Some(offer);
    }
    None
}

/// The words that open another statement: one after a `DECLARE` means the
/// caret has left it.
const STATEMENT_WORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "SET",
    "EXEC",
    "EXECUTE",
    "IF",
    "WHILE",
    "BEGIN",
    "PRINT",
    "RETURN",
    "END",
    "CURSOR",
    "HANDLER",
    "CONDITION",
];

fn declare(cx: &Cx<'_>, t: &[Tok<'_>], base: usize) -> Option<Offer> {
    let segment = after_last_comma(&t[1..], base);
    let words = words_at(segment, base);
    if words.iter().any(|w| STATEMENT_WORDS.contains(w)) {
        return None;
    }
    let end = end_depth(segment, base);
    if end > base {
        // `DECLARE @t TABLE (…)` declares columns.
        let open = segment
            .iter()
            .position(|tok| tok.kind == ScanTokenKind::LeftParen && tok.depth == base)?;
        if last_word(&segment[..open]) == "TABLE" {
            return column_list(cx, &segment[open + 1..], base + 1, ColumnMode::Create);
        }
        if segment.iter().any(is_operator) || words.contains(&"DEFAULT") {
            return None;
        }
        return if open >= 1 && end == base + 1 {
            type_arguments(cx, last_word(&segment[..open]))
        } else {
            None
        };
    }
    if segment.iter().any(is_operator) || words.contains(&"DEFAULT") {
        // The initial value's expression.
        return None;
    }
    match words.as_slice() {
        [] => Offer::nothing(),
        [_] | [_, "AS"] => {
            if cx.tsql() {
                Offer::types(&["TABLE", "CURSOR"])
            } else {
                Offer::types(&[])
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

fn common_types(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => &[
            "INT",
            "BIGINT",
            "NVARCHAR",
            "VARCHAR",
            "BIT",
            "DATETIME2",
            "DATE",
            "DECIMAL",
            "UNIQUEIDENTIFIER",
        ],
        Dialect::Postgres => &[
            "INTEGER",
            "BIGINT",
            "TEXT",
            "VARCHAR",
            "BOOLEAN",
            "TIMESTAMPTZ",
            "NUMERIC",
            "UUID",
            "JSONB",
        ],
        Dialect::MySql => &[
            "INT", "BIGINT", "VARCHAR", "TEXT", "DATETIME", "DECIMAL", "TINYINT", "JSON",
        ],
        Dialect::Oracle => &["NUMBER", "VARCHAR2", "DATE", "TIMESTAMP", "CLOB"],
        Dialect::Sqlite => &["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC"],
    }
}

fn class_of(kind: &ObjectKind) -> Option<(Class, CompletionKind)> {
    match kind {
        ObjectKind::Table => Some((Class::Table, CompletionKind::Table)),
        ObjectKind::View => Some((Class::View, CompletionKind::Table)),
        ObjectKind::StoredProcedure => Some((Class::Procedure, CompletionKind::Procedure)),
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            Some((Class::Function, CompletionKind::Function))
        }
        ObjectKind::Type => Some((Class::Type, CompletionKind::Type)),
        _ => None,
    }
}

/// The schema and the partial name of a dotted prefix (`dbo.Ord` → `dbo`,
/// `Ord`), as the dotted object path reads it.
fn dotted_prefix(prefix: &str) -> Option<(&str, &str)> {
    if prefix.starts_with('@') {
        return None;
    }
    let (path, partial) = prefix.rsplit_once('.')?;
    let schema = path.rsplit('.').next().unwrap_or(path);
    Some((schema.trim_matches(|ch| ch == '[' || ch == ']'), partial))
}

struct ObjectSink<'a> {
    items: &'a mut Vec<CompletionItem>,
    prefix: &'a str,
    dotted: Option<(&'a str, &'a str)>,
    buf: String,
}

impl ObjectSink<'_> {
    /// Offers `schema.name`, matched the way a FROM source is: a dotted
    /// prefix narrows to its schema and inserts the bare name after the dot.
    fn push(
        &mut self,
        schema: Option<&str>,
        name: &str,
        kind: CompletionKind,
        source: CompletionSource,
    ) {
        use std::fmt::Write;
        if let Some((typed_schema, partial)) = self.dotted {
            let Some(schema) = schema.filter(|s| s.eq_ignore_ascii_case(typed_schema)) else {
                return;
            };
            let score = if partial.is_empty() {
                Some(0)
            } else {
                fuzzy::score(partial, name)
            };
            if let Some(score) = score {
                self.buf.clear();
                let _ = write!(self.buf, "{schema}.{name}");
                push_scored(
                    self.items,
                    &self.buf,
                    name,
                    kind,
                    source,
                    OBJECT_SCORE + score,
                );
            }
            return;
        }
        if let Some(score) =
            qualified_object_score_qualified(self.prefix, schema, name, &mut self.buf)
        {
            let label = match schema {
                Some(_) => self.buf.as_str(),
                None => name,
            };
            push_scored(self.items, label, label, kind, source, OBJECT_SCORE + score);
        }
    }
}

fn build(cx: &Cx<'_>, cursor: usize, offer: Offer) -> Vec<CompletionItem> {
    reset_push_cap_state();
    let prefix = cx.prefix;
    let mut items = Vec::new();
    for (rank, keyword) in offer.keywords.iter().enumerate() {
        push_match(
            &mut items,
            keyword,
            keyword,
            CompletionKind::Keyword,
            CompletionSource::Keyword,
            KEYWORD_SCORE - rank as i32,
            prefix,
        );
    }
    for column in &offer.columns {
        if let Some(score) = qualified_object_score(prefix, column) {
            push_scored(
                &mut items,
                column,
                column,
                CompletionKind::Column,
                CompletionSource::LiveMetadata,
                COLUMN_SCORE + score,
            );
        }
    }
    if offer.types && !prefix.contains('.') {
        let common = common_types(cx.dialect);
        for doc in builtins(cx.dialect).type_docs() {
            let name = doc.name.to_ascii_uppercase();
            let base = match common.iter().position(|c| *c == name) {
                Some(rank) => COMMON_TYPE_SCORE - rank as i32,
                None => TYPE_SCORE,
            };
            push_match(
                &mut items,
                &name,
                &name,
                CompletionKind::Type,
                CompletionSource::Builtin,
                base,
                prefix,
            );
        }
    }
    if offer.expression && !prefix.contains('.') {
        let first_function = items.len();
        super::add_builtin_function_items(&mut items, cx.dialect, prefix);
        // A default is nearly always "now", a new id or the current user:
        // those lead instead of the alphabet's `ABS`.
        if offer.columns.is_empty() {
            let usual = usual_defaults(cx.dialect);
            for item in &mut items[first_function..] {
                if let Some(rank) = usual
                    .iter()
                    .position(|name| item.label.eq_ignore_ascii_case(name))
                {
                    item.score = item.score.max(COMMON_TYPE_SCORE - rank as i32);
                }
            }
        }
        // Operators only once a word is being typed: an empty slot starts
        // with an operand.
        let operators: &[&str] = if prefix.is_empty() {
            &["NULL"]
        } else {
            &["NULL", "AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "IS"]
        };
        {
            for keyword in operators.iter().copied() {
                push_match(
                    &mut items,
                    keyword,
                    keyword,
                    CompletionKind::Keyword,
                    CompletionSource::Keyword,
                    TYPE_SCORE,
                    prefix,
                );
            }
        }
    }
    if offer.script_tables {
        add_script_table_drop_items(&mut items, cx.sql, cursor, prefix);
    }
    if offer.databases {
        for database in &cx.catalog.databases {
            push_match(
                &mut items,
                database,
                database,
                CompletionKind::Database,
                CompletionSource::LiveMetadata,
                25,
                prefix,
            );
        }
    }
    if offer.schemas {
        for schema in &cx.catalog.schemas {
            push_match(
                &mut items,
                schema,
                schema,
                CompletionKind::Schema,
                CompletionSource::LiveMetadata,
                30,
                prefix,
            );
        }
        for schema in &cx.catalog.filesystem_schemas {
            push_match(
                &mut items,
                &schema.node.name,
                &schema.node.name,
                CompletionKind::Schema,
                CompletionSource::Filesystem,
                28,
                prefix,
            );
        }
    }
    if !offer.classes.is_empty() {
        let wants = |class: Class| offer.classes.contains(&class);
        // User-defined types are offered beside the built-in ones at a
        // lower rank, objects everywhere else at the usual one.
        let mut sink = ObjectSink {
            items: &mut items,
            prefix,
            dotted: dotted_prefix(prefix),
            buf: String::new(),
        };
        for object in &cx.catalog.objects {
            let Some((class, kind)) = class_of(&object.kind) else {
                continue;
            };
            if wants(class) {
                sink.push(
                    object.schema.as_deref(),
                    &object.name,
                    kind,
                    CompletionSource::LiveMetadata,
                );
            }
        }
        for schema in &cx.catalog.filesystem_schemas {
            let name = Some(schema.node.name.as_str());
            let source = CompletionSource::Filesystem;
            if wants(Class::Table) {
                for table in &schema.tables {
                    sink.push(
                        name,
                        &table.source_node.node.name,
                        CompletionKind::Table,
                        source.clone(),
                    );
                }
            }
            if wants(Class::View) {
                for view in &schema.views {
                    sink.push(
                        name,
                        &view.source_node.node.name,
                        CompletionKind::Table,
                        source.clone(),
                    );
                }
            }
            if wants(Class::Procedure) {
                for procedure in &schema.procedures {
                    sink.push(
                        name,
                        &procedure.operation.source_node.node.name,
                        CompletionKind::Procedure,
                        source.clone(),
                    );
                }
            }
            if wants(Class::Function) {
                for function in &schema.functions {
                    sink.push(
                        name,
                        &function.operation.source_node.node.name,
                        CompletionKind::Function,
                        source.clone(),
                    );
                }
            }
            if wants(Class::Type) {
                for custom in &schema.types {
                    if custom.source_node.node.kind == sqlly_lang::fsmodel::SqlNodeKind::Type {
                        sink.push(
                            name,
                            &custom.source_node.node.name,
                            CompletionKind::Type,
                            source.clone(),
                        );
                    }
                }
            }
        }
    }
    finalize_completion_items(&mut items);
    items
}
