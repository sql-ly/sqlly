//! Shared building blocks for code generation from SQL metadata: the SQL
//! type → language-neutral category mapping, engine type-name parsing, and
//! identifier casing/sanitizing. Used by the result-set code exporter
//! (`result_export_code`) and the schema model generator (`model_codegen`)
//! so the two never disagree about what `nvarchar(50)` or `first_name` mean.

use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

/// Language-neutral bucket for a SQL data type. Fine-grained enough for typed
/// models (dates, decimals, binary, JSON, UUID each get their own idiomatic
/// type per language); [`Self::coarse`] folds it down for the row exporter,
/// which only distinguishes numbers, booleans, and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlTypeCategory {
    /// 32-bit or narrower integers (`int`, `smallint`, `tinyint`, `serial`).
    Int,
    /// 64-bit integers (`bigint`, `int8`, `bigserial`).
    BigInt,
    /// Binary floating point (`float`, `real`, `double precision`).
    Float,
    /// Exact decimals (`decimal`, `numeric`, `money`).
    Decimal,
    Bool,
    /// Character data and anything with no better home.
    Text,
    /// Date + time of day, with or without an offset.
    DateTime,
    /// Date + time of day carrying a UTC offset (`datetimeoffset`,
    /// `timestamptz`).
    DateTimeOffset,
    Date,
    Time,
    Binary,
    Json,
    Uuid,
}

/// The three-way split the row exporter works with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoarseCategory {
    Int,
    Float,
    Bool,
    Text,
}

impl SqlTypeCategory {
    pub fn coarse(self) -> CoarseCategory {
        match self {
            SqlTypeCategory::Int | SqlTypeCategory::BigInt => CoarseCategory::Int,
            SqlTypeCategory::Float | SqlTypeCategory::Decimal => CoarseCategory::Float,
            SqlTypeCategory::Bool => CoarseCategory::Bool,
            _ => CoarseCategory::Text,
        }
    }

    /// True for the temporal categories.
    pub fn is_temporal(self) -> bool {
        matches!(
            self,
            SqlTypeCategory::DateTime
                | SqlTypeCategory::DateTimeOffset
                | SqlTypeCategory::Date
                | SqlTypeCategory::Time
        )
    }
}

/// A parsed engine type spelling: lowercase base name plus the parenthesised
/// arguments (`nvarchar(50)` → `("nvarchar", ["50"])`, `decimal(10, 2)` →
/// `("decimal", ["10", "2"])`, `int unsigned` → `("int", [])` with
/// `unsigned` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedType {
    pub base: String,
    pub args: Vec<String>,
    pub unsigned: bool,
}

impl ParsedType {
    /// First argument as a number (`varchar(50)` → 50). `None` for `max`,
    /// missing, or non-numeric arguments.
    pub fn length(&self) -> Option<u32> {
        self.args.first().and_then(|a| a.parse().ok())
    }

    /// `(precision, scale)` for `decimal(10,2)`-style spellings.
    pub fn precision_scale(&self) -> Option<(u32, u32)> {
        let p = self.args.first()?.parse().ok()?;
        let s = self.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Some((p, s))
    }

    /// True when the spelling is an unbounded variant (`varchar(max)`).
    pub fn is_max(&self) -> bool {
        self.args.first().is_some_and(|a| a == "max")
    }
}

/// Parse an engine type spelling. Never fails: an empty or placeholder type
/// yields an empty base.
pub fn parse_type(data_type: &str) -> ParsedType {
    let lower = data_type.trim().to_lowercase();
    let (head, tail) = match lower.find('(') {
        Some(i) => (&lower[..i], &lower[i + 1..]),
        None => (lower.as_str(), ""),
    };
    let args: Vec<String> = match tail.find(')') {
        Some(end) => tail[..end]
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect(),
        None => Vec::new(),
    };
    // Anything after the closing paren (`int(11) unsigned`) joins the base
    // words so the unsigned marker is seen either way.
    let after = tail.find(')').map(|end| &tail[end + 1..]).unwrap_or("");
    let mut words: Vec<&str> = head.split_whitespace().collect();
    words.extend(after.split_whitespace());
    let unsigned = words.contains(&"unsigned");
    let base = words
        .into_iter()
        .filter(|w| *w != "unsigned" && *w != "zerofill")
        .collect::<Vec<_>>()
        .join(" ");
    let base = if base == "(unknown)" || base == "(col)" {
        String::new()
    } else {
        base
    };
    ParsedType {
        base,
        args,
        unsigned,
    }
}

/// Categorize a type spelling with no engine context. SQL Server's binary
/// `timestamp` is the only spelling whose meaning depends on the engine; it
/// is read as a date/time here (the PostgreSQL/MySQL meaning) — use
/// [`categorize_for`] when the engine is known.
pub fn categorize(data_type: &str) -> SqlTypeCategory {
    categorize_parsed(&parse_type(data_type), None)
}

/// Categorize a type spelling as `db` would read it.
pub fn categorize_for(db: DatabaseType, data_type: &str) -> SqlTypeCategory {
    categorize_parsed(&parse_type(data_type), Some(db))
}

fn categorize_parsed(parsed: &ParsedType, db: Option<DatabaseType>) -> SqlTypeCategory {
    use SqlTypeCategory as C;
    let base = parsed.base.as_str();
    if base.is_empty() {
        return C::Text;
    }
    let family = db.map(|d| d.family());
    // Array spellings (`integer[]`, `_int4`) carry their elements' type but
    // no scalar language type fits; text is the honest fallback.
    if base.ends_with("[]") || base.starts_with('_') {
        return C::Text;
    }
    match base {
        "bool" | "boolean" => return C::Bool,
        // SQL Server `bit` is the boolean; PostgreSQL `bit(n)` is a bit
        // string, but a bare `bit`/`bit(1)` reads as a flag on every engine.
        "bit" if parsed.args.is_empty() || parsed.args.first().is_some_and(|a| a == "1") => {
            return C::Bool;
        }
        // MySQL convention: `tinyint(1)` is the boolean.
        "tinyint" if parsed.args.first().is_some_and(|a| a == "1") => return C::Bool,
        "tinyint" | "smallint" | "int" | "integer" | "mediumint" | "int2" | "int4" | "serial"
        | "smallserial" | "serial2" | "serial4" | "year" => return C::Int,
        "bigint" | "int8" | "bigserial" | "serial8" => return C::BigInt,
        "real" | "float4" | "float" | "double" | "double precision" | "float8" | "binary_float"
        | "binary_double" => return C::Float,
        "decimal" | "numeric" | "dec" | "fixed" | "money" | "smallmoney" | "number" => {
            return C::Decimal;
        }
        "date" => return C::Date,
        "time" | "timetz" | "time without time zone" | "time with time zone" => return C::Time,
        "datetimeoffset" | "timestamptz" | "timestamp with time zone" => return C::DateTimeOffset,
        "datetime" | "datetime2" | "smalldatetime" | "timestamp without time zone" => {
            return C::DateTime;
        }
        // SQL Server's `timestamp` is a row version (binary); everywhere
        // else it is a date/time.
        "timestamp" => {
            return if family == Some(Dialect::SqlServer) {
                C::Binary
            } else {
                C::DateTime
            };
        }
        "rowversion" | "binary" | "varbinary" | "image" | "bytea" | "blob" | "tinyblob"
        | "mediumblob" | "longblob" | "raw" | "long raw" | "bit varying" | "varbit" => {
            return C::Binary;
        }
        "json" | "jsonb" => return C::Json,
        "uuid" | "uniqueidentifier" => return C::Uuid,
        _ => {}
    }
    // Loose fallbacks for spellings not listed above (`integer unsigned`,
    // `character varying`, `long varchar`, driver-reported names).
    if base.contains("json") {
        C::Json
    } else if base.contains("blob") || base.contains("binary") {
        C::Binary
    } else if base.contains("bigint") {
        C::BigInt
    } else if base.contains("int") && !base.contains("point") && !base.contains("interval") {
        C::Int
    } else if base.contains("bool") {
        C::Bool
    } else if base.contains("decimal") || base.contains("numeric") || base.contains("money") {
        C::Decimal
    } else if base.contains("float") || base.contains("double") || base.contains("real") {
        C::Float
    } else if base.contains("datetime") || base.contains("timestamp") {
        C::DateTime
    } else {
        C::Text
    }
}

/// Identifier case convention for a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentCase {
    Snake,
    Camel,
    Pascal,
}

/// Reserved words across the supported languages. Sanitized identifiers
/// colliding with any of these get a trailing underscore. The check is
/// case-insensitive over the union of every target language's keywords, so
/// it over-mangles in languages where the cased identifier would have been
/// legal (e.g. C#'s Pascal-case `Class`) — harmless, and it keeps one
/// column name mapping to one identifier shape everywhere.
pub const RESERVED: &[&str] = &[
    "abstract",
    "and",
    "any",
    "as",
    "assert",
    "associatedtype",
    "async",
    "auto",
    "await",
    "become",
    "bool",
    "boolean",
    "box",
    "break",
    "byte",
    "case",
    "catch",
    "chan",
    "char",
    "class",
    "const",
    "constexpr",
    "continue",
    "crate",
    "data",
    "debugger",
    "declare",
    "def",
    "default",
    "defer",
    "deinit",
    "del",
    "delete",
    "do",
    "double",
    "dyn",
    "elif",
    "else",
    "enum",
    "error",
    "except",
    "explicit",
    "extends",
    "extension",
    "extern",
    "fallthrough",
    "false",
    "final",
    "finally",
    "float",
    "fn",
    "for",
    "friend",
    "from",
    "fun",
    "func",
    "function",
    "global",
    "go",
    "goto",
    "guard",
    "if",
    "impl",
    "implements",
    "import",
    "in",
    "init",
    "inline",
    "inout",
    "instanceof",
    "int",
    "interface",
    "internal",
    "is",
    "keyof",
    "lambda",
    "let",
    "long",
    "loop",
    "map",
    "match",
    "mod",
    "move",
    "mut",
    "mutable",
    "namespace",
    "native",
    "never",
    "new",
    "nil",
    "none",
    "nonlocal",
    "not",
    "null",
    "number",
    "object",
    "of",
    "open",
    "operator",
    "or",
    "override",
    "package",
    "pass",
    "priv",
    "private",
    "protected",
    "protocol",
    "pub",
    "public",
    "raise",
    "range",
    "readonly",
    "record",
    "ref",
    "register",
    "repeat",
    "rethrows",
    "return",
    "sealed",
    "select",
    "self",
    "short",
    "signed",
    "sizeof",
    "static",
    "strictfp",
    "string",
    "struct",
    "subscript",
    "super",
    "switch",
    "synchronized",
    "template",
    "this",
    "throw",
    "throws",
    "trait",
    "transient",
    "true",
    "try",
    "type",
    "typealias",
    "typename",
    "typeof",
    "undefined",
    "union",
    "unknown",
    "unsafe",
    "unsigned",
    "use",
    "val",
    "var",
    "virtual",
    "void",
    "volatile",
    "when",
    "where",
    "while",
    "with",
    "yield",
];

/// Splits a name into lowercase-able words, breaking on non-alphanumeric
/// characters and on lower-to-upper case transitions.
pub fn words_of(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower_or_digit && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
            current.push(ch);
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_lower_or_digit = false;
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn capitalize(word: &str) -> String {
    let lower = word.to_lowercase();
    let mut chars = lower.chars();
    let mut out = String::new();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

/// Join pre-split words in the given case convention. Empty input yields an
/// empty string (callers pick their own fallback).
pub fn join_words(words: &[String], case: IdentCase) -> String {
    match case {
        IdentCase::Snake => words
            .iter()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join("_"),
        IdentCase::Camel => {
            let mut out = String::new();
            for (i, w) in words.iter().enumerate() {
                if i == 0 {
                    out.push_str(&w.to_lowercase());
                } else {
                    out.push_str(&capitalize(w));
                }
            }
            out
        }
        IdentCase::Pascal => words.iter().map(|w| capitalize(w)).collect(),
    }
}

/// Sanitizes a column name into a valid identifier for the given case
/// convention. Empty results become `"column"`; leading digits get a `c`
/// prefix; reserved words get a trailing underscore.
pub fn sanitize_ident(name: &str, case: IdentCase) -> String {
    let words = words_of(name);
    let ident = if words.is_empty() {
        "column".to_string()
    } else {
        join_words(&words, case)
    };
    let ident = if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("c{}", ident)
    } else {
        ident
    };
    if RESERVED.contains(&ident.to_lowercase().as_str()) {
        format!("{}_", ident)
    } else {
        ident
    }
}

/// Make each identifier unique by appending `_2`, `_3`, … to repeats, in
/// order. Two columns that sanitize to the same identifier (`a b` and `a_b`)
/// must not produce one field.
pub fn unique_idents(idents: Vec<String>) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    idents
        .into_iter()
        .map(|base| {
            let mut ident = base.clone();
            let mut suffix = 2;
            while !used.insert(ident.clone()) {
                ident = format!("{}_{}", base, suffix);
                suffix += 1;
            }
            ident
        })
        .collect()
}

/// A single English word to its singular, for turning a plural table name
/// (`Orders`, `Categories`, `Addresses`) into a type name (`Order`,
/// `Category`, `Address`). Conservative: words ending in `ss`/`us`/`is`
/// and short words are left alone.
pub fn singularize(word: &str) -> String {
    let lower = word.to_lowercase();
    if lower.len() <= 3
        || lower.ends_with("ss")
        || lower.ends_with("us")
        || lower.ends_with("is")
        || lower.ends_with("news")
    {
        return word.to_string();
    }
    if let Some(stem) = lower.strip_suffix("ies") {
        return format!("{}y", &word[..stem.len()]);
    }
    for suffix in ["sses", "shes", "ches", "xes", "zes"] {
        if lower.ends_with(suffix) {
            return word[..word.len() - 2].to_string();
        }
    }
    if lower.ends_with('s') {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

/// PascalCase type name for a table: words split, the last one
/// singularized, reserved words and leading digits handled. Empty names
/// become `Record`.
pub fn type_name_for(table: &str) -> String {
    let mut words = words_of(table);
    if let Some(last) = words.last_mut() {
        *last = singularize(last);
    }
    if words.is_empty() {
        return "Record".to_string();
    }
    let ident = join_words(&words, IdentCase::Pascal);
    let ident = if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("T{}", ident)
    } else {
        ident
    };
    if RESERVED.contains(&ident.to_lowercase().as_str()) {
        format!("{}Model", ident)
    } else {
        ident
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_type_splits_base_and_args() {
        assert_eq!(
            parse_type("nvarchar(50)"),
            ParsedType {
                base: "nvarchar".into(),
                args: vec!["50".into()],
                unsigned: false
            }
        );
        assert_eq!(
            parse_type("decimal(10, 2)").precision_scale(),
            Some((10, 2))
        );
        assert!(parse_type("varchar(max)").is_max());
        assert_eq!(parse_type("varchar(max)").length(), None);
        let p = parse_type("int(11) unsigned");
        assert_eq!(p.base, "int");
        assert!(p.unsigned);
        assert_eq!(parse_type("INT UNSIGNED").base, "int");
        assert_eq!(
            parse_type("timestamp without time zone").base,
            "timestamp without time zone"
        );
        assert_eq!(parse_type("(unknown)").base, "");
        assert_eq!(parse_type("").base, "");
    }

    #[test]
    fn categorize_sql_server_types() {
        use SqlTypeCategory as C;
        let db = DatabaseType::SqlServer;
        assert_eq!(categorize_for(db, "int"), C::Int);
        assert_eq!(categorize_for(db, "bigint"), C::BigInt);
        assert_eq!(categorize_for(db, "tinyint"), C::Int);
        assert_eq!(categorize_for(db, "bit"), C::Bool);
        assert_eq!(categorize_for(db, "decimal(10,2)"), C::Decimal);
        assert_eq!(categorize_for(db, "money"), C::Decimal);
        assert_eq!(categorize_for(db, "float"), C::Float);
        assert_eq!(categorize_for(db, "real"), C::Float);
        assert_eq!(categorize_for(db, "datetime2(7)"), C::DateTime);
        assert_eq!(categorize_for(db, "smalldatetime"), C::DateTime);
        assert_eq!(categorize_for(db, "datetimeoffset"), C::DateTimeOffset);
        assert_eq!(categorize_for(db, "date"), C::Date);
        assert_eq!(categorize_for(db, "time(7)"), C::Time);
        assert_eq!(categorize_for(db, "timestamp"), C::Binary);
        assert_eq!(categorize_for(db, "rowversion"), C::Binary);
        assert_eq!(categorize_for(db, "varbinary(max)"), C::Binary);
        assert_eq!(categorize_for(db, "image"), C::Binary);
        assert_eq!(categorize_for(db, "uniqueidentifier"), C::Uuid);
        assert_eq!(categorize_for(db, "nvarchar(50)"), C::Text);
        assert_eq!(categorize_for(db, "xml"), C::Text);
        assert_eq!(categorize_for(db, "geography"), C::Text);
    }

    #[test]
    fn categorize_postgres_types() {
        use SqlTypeCategory as C;
        let db = DatabaseType::Postgres;
        assert_eq!(categorize_for(db, "integer"), C::Int);
        assert_eq!(categorize_for(db, "int4"), C::Int);
        assert_eq!(categorize_for(db, "serial"), C::Int);
        assert_eq!(categorize_for(db, "bigserial"), C::BigInt);
        assert_eq!(categorize_for(db, "int8"), C::BigInt);
        assert_eq!(categorize_for(db, "boolean"), C::Bool);
        assert_eq!(categorize_for(db, "numeric(12,4)"), C::Decimal);
        assert_eq!(categorize_for(db, "double precision"), C::Float);
        assert_eq!(categorize_for(db, "timestamp"), C::DateTime);
        assert_eq!(
            categorize_for(db, "timestamp without time zone"),
            C::DateTime
        );
        assert_eq!(
            categorize_for(db, "timestamp with time zone"),
            C::DateTimeOffset
        );
        assert_eq!(categorize_for(db, "timestamptz"), C::DateTimeOffset);
        assert_eq!(categorize_for(db, "time with time zone"), C::Time);
        assert_eq!(categorize_for(db, "bytea"), C::Binary);
        assert_eq!(categorize_for(db, "jsonb"), C::Json);
        assert_eq!(categorize_for(db, "uuid"), C::Uuid);
        assert_eq!(categorize_for(db, "character varying(20)"), C::Text);
        assert_eq!(categorize_for(db, "interval"), C::Text);
        assert_eq!(categorize_for(db, "integer[]"), C::Text);
        assert_eq!(categorize_for(db, "_int4"), C::Text);
        assert_eq!(categorize_for(db, "point"), C::Text);
    }

    #[test]
    fn categorize_mysql_types() {
        use SqlTypeCategory as C;
        let db = DatabaseType::MySql;
        assert_eq!(categorize_for(db, "int(11)"), C::Int);
        assert_eq!(categorize_for(db, "int unsigned"), C::Int);
        assert_eq!(categorize_for(db, "tinyint(1)"), C::Bool);
        assert_eq!(categorize_for(db, "tinyint(4)"), C::Int);
        assert_eq!(categorize_for(db, "mediumint"), C::Int);
        assert_eq!(categorize_for(db, "bigint(20) unsigned"), C::BigInt);
        assert_eq!(categorize_for(db, "double"), C::Float);
        assert_eq!(categorize_for(db, "decimal(8,2)"), C::Decimal);
        assert_eq!(categorize_for(db, "datetime"), C::DateTime);
        assert_eq!(categorize_for(db, "timestamp"), C::DateTime);
        assert_eq!(categorize_for(db, "year"), C::Int);
        assert_eq!(categorize_for(db, "longblob"), C::Binary);
        assert_eq!(categorize_for(db, "json"), C::Json);
        assert_eq!(categorize_for(db, "enum('a','b')"), C::Text);
        assert_eq!(categorize_for(db, "longtext"), C::Text);
        assert_eq!(categorize_for(DatabaseType::MariaDb, "bit(1)"), C::Bool);
    }

    #[test]
    fn categorize_sqlite_affinities() {
        use SqlTypeCategory as C;
        let db = DatabaseType::Sqlite;
        assert_eq!(categorize_for(db, "INTEGER"), C::Int);
        assert_eq!(categorize_for(db, "REAL"), C::Float);
        assert_eq!(categorize_for(db, "TEXT"), C::Text);
        assert_eq!(categorize_for(db, "BLOB"), C::Binary);
        assert_eq!(categorize_for(db, "NUMERIC"), C::Decimal);
        assert_eq!(categorize_for(db, "BOOLEAN"), C::Bool);
        assert_eq!(categorize_for(db, "DATETIME"), C::DateTime);
        assert_eq!(categorize_for(db, ""), C::Text);
    }

    #[test]
    fn coarse_folds_to_exporter_buckets() {
        assert_eq!(categorize("bigint").coarse(), CoarseCategory::Int);
        assert_eq!(categorize("decimal(10,2)").coarse(), CoarseCategory::Float);
        assert_eq!(categorize("bit").coarse(), CoarseCategory::Bool);
        assert_eq!(categorize("datetime2").coarse(), CoarseCategory::Text);
        assert_eq!(categorize("varbinary(max)").coarse(), CoarseCategory::Text);
        assert_eq!(categorize("(unknown)").coarse(), CoarseCategory::Text);
    }

    #[test]
    fn sanitize_ident_cases() {
        assert_eq!(sanitize_ident("First Name", IdentCase::Snake), "first_name");
        assert_eq!(sanitize_ident("user-id", IdentCase::Camel), "userId");
        assert_eq!(sanitize_ident("user id", IdentCase::Pascal), "UserId");
        assert_eq!(sanitize_ident("firstName", IdentCase::Snake), "first_name");
        assert_eq!(sanitize_ident("firstName", IdentCase::Pascal), "FirstName");
        assert_eq!(sanitize_ident("123", IdentCase::Snake), "c123");
        assert_eq!(sanitize_ident("", IdentCase::Snake), "column");
        assert_eq!(sanitize_ident("  ", IdentCase::Camel), "column");
    }

    #[test]
    fn sanitize_ident_reserved_words_get_underscore() {
        assert_eq!(sanitize_ident("type", IdentCase::Snake), "type_");
        assert_eq!(sanitize_ident("class", IdentCase::Camel), "class_");
        assert_eq!(sanitize_ident("func", IdentCase::Pascal), "Func_");
        assert_eq!(sanitize_ident("lambda", IdentCase::Snake), "lambda_");
        assert_eq!(sanitize_ident("guard", IdentCase::Camel), "guard_");
    }

    #[test]
    fn unique_idents_suffixes_repeats() {
        let out = unique_idents(vec!["a_b".into(), "a_b".into(), "a_b".into(), "c".into()]);
        assert_eq!(out, vec!["a_b", "a_b_2", "a_b_3", "c"]);
    }

    #[test]
    fn singularize_common_plurals() {
        assert_eq!(singularize("Orders"), "Order");
        assert_eq!(singularize("Categories"), "Category");
        assert_eq!(singularize("Addresses"), "Address");
        assert_eq!(singularize("Boxes"), "Box");
        assert_eq!(singularize("Address"), "Address");
        assert_eq!(singularize("Status"), "Status");
        assert_eq!(singularize("Analysis"), "Analysis");
        assert_eq!(singularize("News"), "News");
        assert_eq!(singularize("Bus"), "Bus");
        assert_eq!(singularize("User"), "User");
    }

    #[test]
    fn type_name_for_tables() {
        assert_eq!(type_name_for("Users"), "User");
        assert_eq!(type_name_for("order_items"), "OrderItem");
        assert_eq!(type_name_for("customer-addresses"), "CustomerAddress");
        assert_eq!(type_name_for("SalesOrderHeader"), "SalesOrderHeader");
        assert_eq!(type_name_for("2fa_tokens"), "T2faToken");
        assert_eq!(type_name_for("class"), "ClassModel");
        assert_eq!(type_name_for(""), "Record");
        assert_eq!(type_name_for("$$$"), "Record");
    }
}
