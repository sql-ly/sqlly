//! Casing and identifier policy: the keyword category tables backing
//! `KeywordGroupCasing`, keyword/identifier case transforms, and
//! quote-safety decisions.

use crate::sqlformat::prefs::KeywordCase;
use crate::sqlmodel::Dialect;

pub(super) const CAT_CLAUSES: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "by",
    "having",
    "order",
    "limit",
    "offset",
    "fetch",
    "first",
    "next",
    "rows",
    "only",
    "into",
    "with",
    "window",
    "returning",
    "output",
];
pub(super) const CAT_JOINS: &[&str] = &[
    "join", "inner", "left", "right", "full", "outer", "cross", "apply", "lateral", "natural",
];
pub(super) const CAT_SET_OPS: &[&str] = &["union", "intersect", "except", "minus"];
pub(super) const CAT_DML: &[&str] = &[
    "insert", "update", "delete", "merge", "values", "set", "replace", "matched",
];
pub(super) const CAT_DDL: &[&str] = &[
    "create",
    "alter",
    "drop",
    "table",
    "view",
    "index",
    "constraint",
    "primary",
    "foreign",
    "key",
    "references",
    "unique",
    "check",
    "default",
    "identity",
    "clustered",
    "nonclustered",
    "sequence",
    "trigger",
    "schema",
    "domain",
    "extension",
    "materialized",
    "synonym",
    "package",
    "procedure",
    "function",
    "returns",
    "modify",
    "change",
    "column",
];
pub(super) const CAT_TRANSACTION: &[&str] = &["begin", "commit", "rollback", "tran", "transaction"];
pub(super) const CAT_WORD_OPERATORS: &[&str] = &[
    "and", "or", "not", "in", "between", "like", "ilike", "is", "exists", "glob", "regexp",
];

pub(super) const TYPE_WORDS: &[&str] = &[
    "int",
    "integer",
    "bigint",
    "smallint",
    "tinyint",
    "bit",
    "bool",
    "boolean",
    "decimal",
    "numeric",
    "float",
    "real",
    "double",
    "money",
    "smallmoney",
    "char",
    "nchar",
    "varchar",
    "nvarchar",
    "text",
    "ntext",
    "date",
    "time",
    "datetime",
    "datetime2",
    "smalldatetime",
    "datetimeoffset",
    "timestamp",
    "timestamptz",
    "interval",
    "uuid",
    "uniqueidentifier",
    "binary",
    "varbinary",
    "image",
    "blob",
    "clob",
    "nclob",
    "json",
    "jsonb",
    "xml",
    "geography",
    "geometry",
    "serial",
    "bigserial",
    "number",
    "varchar2",
    "nvarchar2",
    "raw",
    "bytea",
];

pub(super) fn word_in(list: &[&str], w: &str) -> bool {
    list.iter().any(|x| w.eq_ignore_ascii_case(x))
}

// ---------------------------------------------------------------------------
// Casing / quoting helpers
// ---------------------------------------------------------------------------

pub(super) fn apply_keyword_case(text: &str, case: KeywordCase, surrounding_upper: bool) -> String {
    match case {
        KeywordCase::Uppercase => text.to_ascii_uppercase(),
        KeywordCase::Lowercase => text.to_ascii_lowercase(),
        KeywordCase::TitleCase => title_case(text),
        KeywordCase::Preserve => text.to_string(),
        KeywordCase::MatchSurroundingStyle => {
            if surrounding_upper {
                text.to_ascii_uppercase()
            } else {
                text.to_ascii_lowercase()
            }
        }
    }
}

pub(super) fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for c in text.chars() {
        if first {
            out.extend(c.to_uppercase());
            first = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// PascalCase each `_`-separated segment, keeping the underscores: unquoted
/// identifiers are case-insensitive in every supported dialect, so changing
/// case is safe but removing underscores would change the identifier.
pub(super) fn pascal_case_identifier(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start_of_segment = true;
    for c in text.chars() {
        if c == '_' {
            out.push(c);
            start_of_segment = true;
        } else if start_of_segment {
            out.extend(c.to_uppercase());
            start_of_segment = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

pub(super) fn plain_identifier_ok(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether stripping the quotes off `"inner"` keeps the identifier resolving
/// to the same object under `dialect`'s case-folding rules.
pub(super) fn safe_to_unquote(inner: &str, dialect: Dialect) -> bool {
    if !plain_identifier_ok(inner) {
        return false;
    }
    if crate::sqllex::dialect_tables::is_keyword(inner, dialect) {
        return false;
    }
    match dialect {
        // Postgres folds unquoted names to lowercase: only already-lowercase
        // quoted names survive unquoting. MySQL table names can be
        // case-sensitive on Linux; be equally conservative.
        Dialect::Postgres | Dialect::MySql => inner == inner.to_ascii_lowercase(),
        // Oracle folds to upper.
        Dialect::Oracle => inner == inner.to_ascii_uppercase(),
        // SQLite and SQL Server (default collation) are case-insensitive.
        Dialect::Sqlite | Dialect::SqlServer => true,
    }
}

pub(super) fn default_quotes(dialect: Dialect) -> (char, char) {
    match dialect {
        Dialect::SqlServer => ('[', ']'),
        Dialect::MySql => ('`', '`'),
        _ => ('"', '"'),
    }
}
