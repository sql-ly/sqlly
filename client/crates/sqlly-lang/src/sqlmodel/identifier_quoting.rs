//! When an identifier must be quoted to be referenced verbatim, per engine.
//!
//! [`DatabaseType::quote_identifier`] always quotes; this module decides
//! whether quoting is *required*: a name that is not a plain identifier
//! (spaces, dashes, a leading digit, other punctuation), a reserved word, or —
//! on engines that fold unquoted identifiers (PostgreSQL lowercases, Oracle
//! uppercases) — a name whose case the fold would change. Completion uses it
//! to insert `dbo.[Order Details]` rather than a bare name that would not
//! parse, while leaving ordinary names like `dbo.Orders` untouched.
//!
//! The checks are allocation-free: they run for every completion item the
//! editor renders.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::sqlmodel::{DatabaseType, Dialect, IdentifierFold};

impl DatabaseType {
    /// How the engine folds *unquoted* identifiers. Differs from
    /// [`Dialect::identifier_fold`] where an engine borrows a grammar family
    /// but not its folding: DuckDB, Spanner, and Databricks ride the Postgres
    /// grammar yet compare names case-insensitively, while Snowflake, Db2,
    /// and SAP HANA fold to upper case.
    pub fn unquoted_identifier_fold(self) -> IdentifierFold {
        use DatabaseType::*;
        match self {
            DuckDb | Spanner | Databricks | Teradata => IdentifierFold::CaseInsensitive,
            Snowflake | Db2 | SapHana => IdentifierFold::Upper,
            Informix => IdentifierFold::Lower,
            _ => self.family().identifier_fold(),
        }
    }

    /// Whether `identifier` must be quoted for the engine to read it as
    /// written. Empty names are reported as not needing quotes (there is
    /// nothing sensible to quote).
    pub fn identifier_needs_quoting(self, identifier: &str) -> bool {
        let mut chars = identifier.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_alphabetic() || first == '_') {
            return true;
        }
        let extra = self.plain_identifier_extra_chars();
        if !chars.all(|c| c.is_alphanumeric() || c == '_' || extra.contains(c)) {
            return true;
        }
        let folds_away = match self.unquoted_identifier_fold() {
            IdentifierFold::Lower => identifier.chars().any(|c| c.is_ascii_uppercase()),
            IdentifierFold::Upper => identifier.chars().any(|c| c.is_ascii_lowercase()),
            IdentifierFold::CaseInsensitive | IdentifierFold::Exact => false,
        };
        folds_away || is_reserved_word(self, identifier)
    }

    /// `identifier` quoted in the engine's style when
    /// [`Self::identifier_needs_quoting`] says so, otherwise borrowed as is.
    pub fn quote_identifier_if_needed(self, identifier: &str) -> Cow<'_, str> {
        if self.identifier_needs_quoting(identifier) {
            Cow::Owned(self.quote_identifier(identifier))
        } else {
            Cow::Borrowed(identifier)
        }
    }

    /// Non-alphanumeric characters an unquoted identifier may contain after
    /// its first character.
    fn plain_identifier_extra_chars(self) -> &'static str {
        use DatabaseType::*;
        match self {
            ClickHouse | DuckDb => "",
            _ => match self.family() {
                Dialect::SqlServer => "@$#",
                Dialect::Oracle => "$#",
                Dialect::Postgres | Dialect::MySql | Dialect::Sqlite => "$",
            },
        }
    }
}

/// Whether `word` is reserved on the engine, so a column or table of that
/// name must be quoted (`[Order]`, `"user"`). Case-insensitive.
pub fn is_reserved_word(db: DatabaseType, word: &str) -> bool {
    // ClickHouse keywords are contextual: `default`, `user`, `table` and the
    // like are everyday bare names there. Only the clause words that would
    // end an expression need quotes.
    if db == DatabaseType::ClickHouse {
        return CLICKHOUSE.iter().any(|w| w.eq_ignore_ascii_case(word));
    }
    is_reserved_in_dialect(db.family(), word)
}

/// [`is_reserved_word`] for a grammar family rather than a specific engine —
/// what the formatter has to go on. Case-insensitive.
pub fn is_reserved_in_dialect(dialect: Dialect, word: &str) -> bool {
    if word.len() > MAX_RESERVED_LEN {
        return false;
    }
    let mut buf = [0u8; MAX_RESERVED_LEN];
    for (dst, b) in buf.iter_mut().zip(word.as_bytes()) {
        *dst = b.to_ascii_lowercase();
    }
    let folded = &buf[..word.len()];
    let sets = reserved_sets();
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

/// Longest reserved word below ("semantickeyphrasetable", 22 bytes) with
/// headroom; longer words can never be reserved and skip the fold.
const MAX_RESERVED_LEN: usize = 32;

fn reserved_sets() -> &'static [HashSet<&'static [u8]>; 6] {
    static SETS: OnceLock<[HashSet<&'static [u8]>; 6]> = OnceLock::new();
    SETS.get_or_init(|| {
        let set = |words: &'static [&'static str]| {
            words.iter().map(|w| w.as_bytes()).collect::<HashSet<_>>()
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

/// The ClickHouse clause words that cannot stand as a bare name.
const CLICKHOUSE: &[&str] = &[
    "and", "as", "by", "from", "group", "having", "in", "join", "limit", "not", "on", "or",
    "order", "select", "union", "where", "with",
];

/// Reserved on every engine we model except ClickHouse (the SQL-standard
/// core that each engine's parser rejects as a bare identifier). Lowercase.
const COMMON: &[&str] = &[
    "all",
    "and",
    "any",
    "as",
    "asc",
    "between",
    "both",
    "by",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "in",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "leading",
    "left",
    "like",
    "natural",
    "not",
    "null",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "select",
    "session_user",
    "set",
    "some",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "when",
    "where",
    "with",
];

/// T-SQL reserved keywords beyond [`COMMON`].
const SQLSERVER: &[&str] = &[
    "add",
    "alter",
    "authorization",
    "backup",
    "begin",
    "break",
    "browse",
    "bulk",
    "cascade",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "commit",
    "compute",
    "contains",
    "containstable",
    "continue",
    "convert",
    "current",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "deny",
    "disk",
    "distributed",
    "double",
    "dump",
    "errlvl",
    "escape",
    "exec",
    "execute",
    "exit",
    "external",
    "file",
    "fillfactor",
    "freetext",
    "freetexttable",
    "function",
    "goto",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "index",
    "key",
    "kill",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "nullif",
    "of",
    "off",
    "offsets",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "setuser",
    "shutdown",
    "statistics",
    "system_user",
    "tablesample",
    "textsize",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "unpivot",
    "updatetext",
    "use",
    "varying",
    "view",
    "waitfor",
    "while",
    "within",
    "writetext",
];

/// PostgreSQL reserved (and reserved-but-function-name) keywords beyond
/// [`COMMON`].
const POSTGRES: &[&str] = &[
    "analyse",
    "analyze",
    "array",
    "asymmetric",
    "authorization",
    "binary",
    "collation",
    "concurrently",
    "current_catalog",
    "current_role",
    "current_schema",
    "deferrable",
    "do",
    "freeze",
    "ilike",
    "initially",
    "isnull",
    "lateral",
    "limit",
    "localtime",
    "localtimestamp",
    "notnull",
    "offset",
    "only",
    "overlaps",
    "placing",
    "returning",
    "similar",
    "symmetric",
    "tablesample",
    "variadic",
    "verbose",
    "window",
];

/// MySQL / MariaDB reserved words beyond [`COMMON`].
const MYSQL: &[&str] = &[
    "accessible",
    "add",
    "alter",
    "analyze",
    "before",
    "bigint",
    "binary",
    "blob",
    "call",
    "cascade",
    "change",
    "char",
    "character",
    "condition",
    "continue",
    "convert",
    "cube",
    "cume_dist",
    "cursor",
    "database",
    "databases",
    "day_hour",
    "day_microsecond",
    "day_minute",
    "day_second",
    "dec",
    "decimal",
    "declare",
    "delayed",
    "dense_rank",
    "describe",
    "deterministic",
    "distinctrow",
    "div",
    "double",
    "dual",
    "each",
    "elseif",
    "empty",
    "enclosed",
    "escaped",
    "exit",
    "explain",
    "first_value",
    "float",
    "float4",
    "float8",
    "force",
    "fulltext",
    "function",
    "generated",
    "get",
    "grouping",
    "groups",
    "high_priority",
    "hour_microsecond",
    "hour_minute",
    "hour_second",
    "if",
    "ignore",
    "index",
    "infile",
    "inout",
    "insensitive",
    "int",
    "int1",
    "int2",
    "int3",
    "int4",
    "int8",
    "integer",
    "interval",
    "iterate",
    "json_table",
    "key",
    "keys",
    "kill",
    "lag",
    "last_value",
    "lateral",
    "lead",
    "leave",
    "limit",
    "linear",
    "lines",
    "load",
    "localtime",
    "localtimestamp",
    "lock",
    "long",
    "longblob",
    "longtext",
    "loop",
    "low_priority",
    "match",
    "maxvalue",
    "mediumblob",
    "mediumint",
    "mediumtext",
    "middleint",
    "minute_microsecond",
    "minute_second",
    "mod",
    "modifies",
    "no_write_to_binlog",
    "nth_value",
    "ntile",
    "numeric",
    "of",
    "optimize",
    "option",
    "optionally",
    "out",
    "outfile",
    "over",
    "partition",
    "percent_rank",
    "precision",
    "procedure",
    "purge",
    "range",
    "rank",
    "read",
    "read_write",
    "reads",
    "real",
    "recursive",
    "regexp",
    "release",
    "rename",
    "repeat",
    "replace",
    "require",
    "resignal",
    "restrict",
    "return",
    "revoke",
    "rlike",
    "row",
    "row_number",
    "rows",
    "schema",
    "schemas",
    "second_microsecond",
    "sensitive",
    "separator",
    "show",
    "signal",
    "smallint",
    "spatial",
    "specific",
    "sql",
    "sql_big_result",
    "sql_calc_found_rows",
    "sql_small_result",
    "sqlexception",
    "sqlstate",
    "sqlwarning",
    "ssl",
    "starting",
    "stored",
    "straight_join",
    "system",
    "terminated",
    "tinyblob",
    "tinyint",
    "tinytext",
    "trigger",
    "undo",
    "unlock",
    "unsigned",
    "usage",
    "use",
    "utc_date",
    "utc_time",
    "utc_timestamp",
    "varbinary",
    "varchar",
    "varcharacter",
    "varying",
    "virtual",
    "while",
    "window",
    "write",
    "xor",
    "year_month",
    "zerofill",
];

/// Oracle reserved words beyond [`COMMON`].
const ORACLE: &[&str] = &[
    "access",
    "add",
    "alter",
    "audit",
    "char",
    "cluster",
    "comment",
    "compress",
    "connect",
    "current",
    "date",
    "decimal",
    "exclusive",
    "file",
    "float",
    "identified",
    "immediate",
    "increment",
    "index",
    "initial",
    "integer",
    "level",
    "lock",
    "long",
    "maxextents",
    "minus",
    "mlslabel",
    "mode",
    "modify",
    "noaudit",
    "nocompress",
    "nowait",
    "number",
    "of",
    "offline",
    "online",
    "option",
    "pctfree",
    "prior",
    "privileges",
    "public",
    "raw",
    "rename",
    "resource",
    "revoke",
    "row",
    "rowid",
    "rownum",
    "rows",
    "session",
    "share",
    "size",
    "smallint",
    "start",
    "successful",
    "synonym",
    "sysdate",
    "uid",
    "validate",
    "varchar",
    "varchar2",
    "view",
    "whenever",
];

/// SQLite keywords beyond [`COMMON`] that cannot stand as a bare name.
const SQLITE: &[&str] = &[
    "add",
    "alter",
    "attach",
    "autoincrement",
    "begin",
    "commit",
    "database",
    "detach",
    "escape",
    "glob",
    "if",
    "index",
    "indexed",
    "isnull",
    "limit",
    "match",
    "notnull",
    "offset",
    "pragma",
    "raise",
    "regexp",
    "reindex",
    "rollback",
    "savepoint",
    "transaction",
    "trigger",
    "vacuum",
    "view",
    "virtual",
];

#[cfg(test)]
mod tests {
    use super::*;
    use DatabaseType::*;

    #[test]
    fn plain_names_stay_unquoted_everywhere() {
        for db in [
            SqlServer, Postgres, MySql, MariaDb, Sqlite, LibSql, DuckDb, ClickHouse,
        ] {
            assert!(!db.identifier_needs_quoting("orders"), "{db:?}");
            assert!(!db.identifier_needs_quoting("order_lines"), "{db:?}");
            assert!(!db.identifier_needs_quoting("_hidden2"), "{db:?}");
        }
        // Oracle stores unquoted names upper-cased, so those are its plain ones.
        assert!(!Oracle.identifier_needs_quoting("ORDERS"));
        assert!(!Oracle.identifier_needs_quoting("ORDER_LINES"));
    }

    #[test]
    fn special_characters_and_leading_digits_need_quotes() {
        for db in [
            SqlServer, Postgres, MySql, MariaDb, Sqlite, LibSql, Oracle, DuckDb, ClickHouse,
        ] {
            assert!(db.identifier_needs_quoting("Order Details"), "{db:?}");
            assert!(db.identifier_needs_quoting("first-name"), "{db:?}");
            assert!(db.identifier_needs_quoting("2019_sales"), "{db:?}");
            assert!(db.identifier_needs_quoting("a.b"), "{db:?}");
            assert!(db.identifier_needs_quoting("we\"ird"), "{db:?}");
        }
    }

    #[test]
    fn reserved_words_need_quotes() {
        for db in [SqlServer, Postgres, MySql, Sqlite, Oracle, ClickHouse] {
            assert!(db.identifier_needs_quoting("order"), "{db:?}");
            assert!(db.identifier_needs_quoting("select"), "{db:?}");
        }
        // ClickHouse keywords are contextual; `default` is its stock database.
        assert!(!ClickHouse.identifier_needs_quoting("default"));
        assert!(!ClickHouse.identifier_needs_quoting("user"));
        assert!(SqlServer.identifier_needs_quoting("default"));
        // Engine-specific reservations.
        assert!(SqlServer.identifier_needs_quoting("Key"));
        assert!(SqlServer.identifier_needs_quoting("Percent"));
        assert!(!Postgres.identifier_needs_quoting("percent"));
        assert!(Postgres.identifier_needs_quoting("limit"));
        assert!(MySql.identifier_needs_quoting("Rank"));
        assert!(Oracle.identifier_needs_quoting("LEVEL"));
        assert!(!SqlServer.identifier_needs_quoting("Level"));
        // Common column names that are only non-reserved keywords stay bare.
        assert!(!SqlServer.identifier_needs_quoting("Type"));
        assert!(!SqlServer.identifier_needs_quoting("Name"));
        assert!(!Postgres.identifier_needs_quoting("name"));
    }

    #[test]
    fn case_folding_engines_quote_names_the_fold_would_change() {
        assert!(Postgres.identifier_needs_quoting("OrderLines"));
        assert!(CockroachDb.identifier_needs_quoting("OrderLines"));
        assert!(!Postgres.identifier_needs_quoting("order_lines"));
        assert!(Oracle.identifier_needs_quoting("OrderLines"));
        assert!(!Oracle.identifier_needs_quoting("ORDER_LINES"));
        assert!(Snowflake.identifier_needs_quoting("lower_name"));
        // Case-insensitive engines never quote for case alone.
        for db in [
            SqlServer, MySql, MariaDb, Sqlite, LibSql, DuckDb, ClickHouse,
        ] {
            assert!(!db.identifier_needs_quoting("OrderLines"), "{db:?}");
        }
    }

    #[test]
    fn engine_specific_identifier_characters() {
        assert!(!SqlServer.identifier_needs_quoting("Price$"));
        assert!(!SqlServer.identifier_needs_quoting("Col#1"));
        assert!(Postgres.identifier_needs_quoting("col#1"));
        assert!(!Postgres.identifier_needs_quoting("price$"));
        assert!(ClickHouse.identifier_needs_quoting("price$"));
        assert!(SqlServer.identifier_needs_quoting("#temp"));
    }

    #[test]
    fn quote_if_needed_uses_the_engine_style_and_escapes() {
        assert_eq!(SqlServer.quote_identifier_if_needed("Orders"), "Orders");
        assert_eq!(
            SqlServer.quote_identifier_if_needed("Order Details"),
            "[Order Details]"
        );
        assert_eq!(SqlServer.quote_identifier_if_needed("a]b c"), "[a]]b c]");
        assert_eq!(MySql.quote_identifier_if_needed("order"), "`order`");
        assert_eq!(MariaDb.quote_identifier_if_needed("we`ird"), "`we``ird`");
        assert_eq!(ClickHouse.quote_identifier_if_needed("my-col"), "`my-col`");
        assert_eq!(
            Postgres.quote_identifier_if_needed("OrderLines"),
            "\"OrderLines\""
        );
        assert_eq!(
            Postgres.quote_identifier_if_needed("we\"ird"),
            "\"we\"\"ird\""
        );
        assert_eq!(Oracle.quote_identifier_if_needed("Mixed"), "\"Mixed\"");
        assert_eq!(Sqlite.quote_identifier_if_needed("order"), "\"order\"");
        assert_eq!(DuckDb.quote_identifier_if_needed("My Col"), "\"My Col\"");
        assert_eq!(DuckDb.quote_identifier_if_needed("MyCol"), "MyCol");
        assert_eq!(LibSql.quote_identifier_if_needed("2x"), "\"2x\"");
    }

    #[test]
    fn empty_and_overlong_names() {
        assert!(!SqlServer.identifier_needs_quoting(""));
        let long = "a".repeat(200);
        assert!(!SqlServer.identifier_needs_quoting(&long));
    }
}
