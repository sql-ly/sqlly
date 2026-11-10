//! Per-dialect data tables: keywords and operators. Dialect quirks are table
//! edits here, not code branches in the lexer.

use crate::sqlmodel::Dialect;

/// Keywords shared by every dialect we model. This is intentionally the
/// reserved/structural subset that parsers dispatch on, not the full reserved
/// word list of each engine.
const COMMON: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "by",
    "having",
    "order",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "alter",
    "drop",
    "table",
    "view",
    "index",
    "on",
    "primary",
    "key",
    "foreign",
    "references",
    "unique",
    "check",
    "constraint",
    "default",
    "not",
    "null",
    "and",
    "or",
    "in",
    "between",
    "like",
    "is",
    "exists",
    "union",
    "all",
    "distinct",
    "as",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "case",
    "when",
    "then",
    "else",
    "end",
    "cast",
    "asc",
    "desc",
    "limit",
    "offset",
    "trigger",
    "begin",
    "commit",
    "rollback",
    "grant",
    "revoke",
    "with",
    "recursive",
];

const SQLSERVER: &[&str] = &[
    "top",
    "exec",
    "execute",
    "procedure",
    "proc",
    "function",
    "returns",
    "declare",
    "go",
    "identity",
    "clustered",
    "nonclustered",
    "nvarchar",
    "varchar",
    "int",
    "bigint",
    "bit",
    "datetime",
    "uniqueidentifier",
    "output",
    "schema",
    "use",
    "merge",
    "pivot",
    "unpivot",
    "synonym",
    "sequence",
    "type",
    "rowcount",
    "tran",
    "transaction",
    "apply",
];

const POSTGRES: &[&str] = &[
    "returning",
    "ilike",
    "serial",
    "bigserial",
    "materialized",
    "concurrently",
    "using",
    "language",
    "plpgsql",
    "extension",
    "domain",
    "enum",
    "inherits",
    "tablespace",
    "vacuum",
    "analyze",
    "lateral",
    "fetch",
    "first",
    "next",
    "rows",
    "only",
    "function",
    "procedure",
    "returns",
    "schema",
    "sequence",
    "type",
    "do",
    "declare",
];

const MYSQL: &[&str] = &[
    "auto_increment",
    "engine",
    "charset",
    "collate",
    "unsigned",
    "zerofill",
    "delimiter",
    "procedure",
    "function",
    "returns",
    "declare",
    "database",
    "databases",
    "show",
    "describe",
    "replace",
    "ignore",
    "duplicate",
    "schema",
    "modify",
    "change",
    "event",
    "enum",
];

const ORACLE: &[&str] = &[
    "number",
    "varchar2",
    "nvarchar2",
    "rownum",
    "dual",
    "package",
    "body",
    "synonym",
    "sequence",
    "nocache",
    "nocycle",
    "minvalue",
    "maxvalue",
    "connect",
    "level",
    "start",
    "prior",
    "procedure",
    "function",
    "returns",
    "return",
    "declare",
    "replace",
    "force",
    "materialized",
    "fetch",
    "first",
    "rows",
    "only",
    "type",
    "schema",
    "is",
];

const SQLITE: &[&str] = &[
    "autoincrement",
    "pragma",
    "attach",
    "detach",
    "vacuum",
    "without",
    "rowid",
    "glob",
    "regexp",
    "temp",
    "temporary",
    "virtual",
    "if",
];

/// Longest keyword in any table ("uniqueidentifier", 16 bytes) with headroom;
/// longer words can never be keywords and skip the fold entirely.
const MAX_KEYWORD_LEN: usize = 24;

/// Hash sets over the tables above, keyed by their (already lowercase) bytes.
/// Index 0 is COMMON; 1.. follow the [`Dialect`] match in [`is_keyword`].
fn keyword_sets() -> &'static [std::collections::HashSet<&'static [u8]>; 6] {
    static SETS: std::sync::OnceLock<[std::collections::HashSet<&'static [u8]>; 6]> =
        std::sync::OnceLock::new();
    SETS.get_or_init(|| {
        let set = |words: &'static [&'static str]| {
            words
                .iter()
                .map(|w| w.as_bytes())
                .collect::<std::collections::HashSet<_>>()
        };
        [
            set(COMMON),
            set(SQLSERVER),
            set(POSTGRES),
            set(MYSQL),
            set(ORACLE),
            set(SQLITE),
        ]
    })
}

/// Allocation-free: the word is case-folded into a stack buffer and looked up
/// by bytes. This sits on the lexer hot path — every word token passes
/// through here — so it must not allocate per call (AGENTS.md).
pub fn is_keyword(word: &str, dialect: Dialect) -> bool {
    if word.len() > MAX_KEYWORD_LEN {
        return false;
    }
    let mut buf = [0u8; MAX_KEYWORD_LEN];
    for (dst, b) in buf.iter_mut().zip(word.as_bytes()) {
        *dst = b.to_ascii_lowercase();
    }
    let folded = &buf[..word.len()];
    let sets = keyword_sets();
    if sets[0].contains(folded) {
        return true;
    }
    let extra = match dialect {
        Dialect::SqlServer => &sets[1],
        Dialect::Postgres => &sets[2],
        Dialect::MySql => &sets[3],
        Dialect::Oracle => &sets[4],
        Dialect::Sqlite => &sets[5],
    };
    extra.contains(folded)
}

pub const OPERATOR_CHARS: &[u8] = b"+-*/%<>=!|&^~:";

const OPERATORS_2: &[&str] = &[
    "<=", ">=", "<>", "!=", "||", "::", ":=", "&&", "<<", ">>", "->", "=>", "!<", "!>", "*=", "+=",
    "-=", "/=", "%=", "==", "**",
];

/// MySQL / MariaDB / SQLite JSON extraction (`->>`) and MySQL's null-safe
/// equality (`<=>`). PostgreSQL operators are lexed as runs instead.
const OPERATORS_3: &[&str] = &["->>", "<=>"];

pub fn is_operator(text: &str) -> bool {
    text.len() == 1 || OPERATORS_2.contains(&text) || OPERATORS_3.contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_dialect_scoped() {
        assert!(is_keyword("SELECT", Dialect::Sqlite));
        assert!(is_keyword("rownum", Dialect::Oracle));
        assert!(!is_keyword("rownum", Dialect::SqlServer));
        assert!(is_keyword("TOP", Dialect::SqlServer));
        assert!(!is_keyword("top", Dialect::Postgres));
        assert!(is_keyword("returning", Dialect::Postgres));
        assert!(!is_keyword("returning", Dialect::SqlServer));
        assert!(is_keyword("auto_increment", Dialect::MySql));
    }

    #[test]
    fn operator_table() {
        assert!(is_operator(">="));
        assert!(is_operator("<>"));
        assert!(is_operator("::"));
        assert!(!is_operator("=>>"));
    }

    /// The folded lookup assumes every table entry is lowercase and within
    /// the stack-buffer bound; a violated entry would silently never match.
    #[test]
    fn keyword_tables_are_lowercase_and_bounded() {
        for table in [COMMON, SQLSERVER, POSTGRES, MYSQL, ORACLE, SQLITE] {
            for word in table {
                assert!(
                    word.len() <= MAX_KEYWORD_LEN,
                    "{word} exceeds MAX_KEYWORD_LEN"
                );
                assert_eq!(*word, word.to_ascii_lowercase(), "{word} is not lowercase");
            }
        }
        // Oversized words bail out before folding.
        assert!(!is_keyword(
            "averyveryverylongidentifierthatcannotbeakeyword",
            Dialect::SqlServer
        ));
    }
}
