//! Source column types → DuckDB column types for notebook materialization.
//!
//! Every driver reports `ResultColumn::data_type` in its own engine's
//! spelling (`nvarchar`, `int4`, `NUMBER(10,2)`, `Nullable(UInt64)`, a SQLite
//! declared type, …). [`local_type_for`] reads that spelling for the source
//! engine and picks the DuckDB type the materialized column gets. Exact
//! numerics keep the precision and scale they were declared with; a decimal
//! with no declared precision is sized from the values themselves
//! ([`LocalType::InferDecimal`]); names nothing here recognizes become
//! VARCHAR. The mapping is a first choice only: ingestion still checks every
//! value converts and keeps a column as VARCHAR when one does not, so a wrong
//! guess never loses data.

use serde::{Deserialize, Serialize};
use sqlly_core::DatabaseType;

/// The DuckDB type a materialized column is created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalType {
    Boolean,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    UTinyInt,
    USmallInt,
    UInteger,
    UBigInt,
    HugeInt,
    UHugeInt,
    Float,
    Double,
    /// DECIMAL(precision, scale), precision 1..=38.
    Decimal {
        precision: u8,
        scale: u8,
    },
    /// An exact numeric whose precision the source did not report: sized to
    /// fit the values after they arrive.
    InferDecimal,
    Varchar,
    Blob,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    Json,
    /// No usable declared type (a SQLite expression column): BIGINT, then
    /// DOUBLE, then VARCHAR, whichever every value converts to.
    Infer,
}

/// DuckDB's widest DECIMAL.
pub const MAX_DECIMAL_PRECISION: u8 = 38;

impl LocalType {
    /// The column type as DuckDB DDL. The two inferring variants have no
    /// spelling of their own until ingestion resolves them; they spell as
    /// VARCHAR, their fallback.
    pub fn sql(self) -> String {
        match self {
            LocalType::Boolean => "BOOLEAN".into(),
            LocalType::TinyInt => "TINYINT".into(),
            LocalType::SmallInt => "SMALLINT".into(),
            LocalType::Integer => "INTEGER".into(),
            LocalType::BigInt => "BIGINT".into(),
            LocalType::UTinyInt => "UTINYINT".into(),
            LocalType::USmallInt => "USMALLINT".into(),
            LocalType::UInteger => "UINTEGER".into(),
            LocalType::UBigInt => "UBIGINT".into(),
            LocalType::HugeInt => "HUGEINT".into(),
            LocalType::UHugeInt => "UHUGEINT".into(),
            LocalType::Float => "FLOAT".into(),
            LocalType::Double => "DOUBLE".into(),
            LocalType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
            LocalType::InferDecimal | LocalType::Infer | LocalType::Varchar => "VARCHAR".into(),
            LocalType::Blob => "BLOB".into(),
            LocalType::Date => "DATE".into(),
            LocalType::Time => "TIME".into(),
            LocalType::TimeTz => "TIMETZ".into(),
            LocalType::Timestamp => "TIMESTAMP".into(),
            LocalType::TimestampTz => "TIMESTAMPTZ".into(),
            LocalType::Interval => "INTERVAL".into(),
            LocalType::Uuid => "UUID".into(),
            LocalType::Json => "JSON".into(),
        }
    }

    /// Whether values are staged as bytes rather than text.
    pub(crate) fn is_blob(self) -> bool {
        self == LocalType::Blob
    }

    /// A DECIMAL when `precision`/`scale` fit DuckDB, else VARCHAR (DuckDB
    /// has no wider exact type).
    fn decimal(precision: u32, scale: u32) -> LocalType {
        match (u8::try_from(precision), u8::try_from(scale)) {
            (Ok(p), Ok(s)) if (1..=MAX_DECIMAL_PRECISION).contains(&p) && s <= p => {
                LocalType::Decimal {
                    precision: p,
                    scale: s,
                }
            }
            _ => LocalType::Varchar,
        }
    }
}

/// How an engine family spells its column types. Engines that speak another
/// engine's wire protocol share its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeFamily {
    SqlServer,
    Postgres,
    MySql,
    Sqlite,
    Oracle,
    DuckDb,
    ClickHouse,
    /// Everything else (Snowflake, Spanner, Databricks, Db2, Redis, …):
    /// the common SQL names.
    Generic,
}

impl TypeFamily {
    pub fn of(database_type: DatabaseType) -> TypeFamily {
        use DatabaseType::*;
        match database_type {
            SqlServer | AzureSqlDatabase | Access => TypeFamily::SqlServer,
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | WarehousePg => TypeFamily::Postgres,
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb => TypeFamily::MySql,
            Sqlite | LibSql => TypeFamily::Sqlite,
            Oracle => TypeFamily::Oracle,
            DuckDb => TypeFamily::DuckDb,
            ClickHouse => TypeFamily::ClickHouse,
            Db2 | Snowflake | Spanner | Teradata | Informix | Databricks | SapHana | Redis
            | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                TypeFamily::Generic
            }
        }
    }
}

/// A type name split into its words (outside parentheses, single-spaced,
/// lower-cased), the arguments of its first parenthesized group, and the
/// MySQL `unsigned` flag.
#[derive(Debug, PartialEq)]
struct TypeName {
    base: String,
    args: Vec<String>,
    unsigned: bool,
}

fn parse(raw: &str) -> TypeName {
    let lowered = unwrap_clickhouse_modifiers(raw.trim().to_ascii_lowercase());
    let mut words = String::new();
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut first_group_done = false;
    let mut current = String::new();
    for c in lowered.chars() {
        match c {
            '(' => {
                if depth > 0 {
                    current.push(c);
                }
                depth += 1;
                words.push(' ');
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if !first_group_done {
                        args.push(current.trim().to_string());
                        first_group_done = true;
                    }
                    current.clear();
                } else {
                    current.push(c);
                }
            }
            ',' if depth == 1 => {
                if !first_group_done {
                    args.push(current.trim().to_string());
                }
                current.clear();
            }
            _ if depth > 0 => current.push(c),
            _ => words.push(c),
        }
    }
    let mut parts: Vec<&str> = words.split_whitespace().collect();
    let unsigned = parts.contains(&"unsigned");
    parts.retain(|w| !matches!(*w, "unsigned" | "signed" | "zerofill"));
    args.retain(|a| !a.is_empty());
    TypeName {
        base: parts.join(" "),
        args,
        unsigned,
    }
}

/// ClickHouse wraps value types in `Nullable(…)` / `LowCardinality(…)`;
/// the wrapped type is what matters.
fn unwrap_clickhouse_modifiers(mut name: String) -> String {
    loop {
        let inner = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .and_then(|rest| rest.strip_suffix(')'))
            .map(|inner| inner.trim().to_string());
        match inner {
            Some(inner) => name = inner,
            None => return name,
        }
    }
}

fn numeric_arg(args: &[String], index: usize) -> Option<u32> {
    args.get(index)?.trim().parse().ok()
}

/// DECIMAL/NUMERIC with optional `(p)` / `(p, s)` arguments.
fn decimal_from_args(args: &[String]) -> LocalType {
    match (numeric_arg(args, 0), args.len()) {
        (Some(precision), 1) => LocalType::decimal(precision, 0),
        (Some(precision), 2) => match numeric_arg(args, 1) {
            Some(scale) => LocalType::decimal(precision, scale),
            None => LocalType::InferDecimal,
        },
        _ => LocalType::InferDecimal,
    }
}

/// The DuckDB type for a column the source engine reported as
/// `source_type`.
pub fn local_type_for(database_type: DatabaseType, source_type: &str) -> LocalType {
    let family = TypeFamily::of(database_type);
    if family == TypeFamily::Sqlite {
        return sqlite_type(source_type);
    }
    let name = parse(source_type);
    if name.base.is_empty() {
        return LocalType::Varchar;
    }
    // Postgres names array types `_int4` and spells them `integer[]`.
    if (family == TypeFamily::Postgres && name.base.starts_with('_')) || name.base.ends_with("[]") {
        return LocalType::Varchar;
    }
    family_specific(family, &name).unwrap_or_else(|| common_type(family, &name))
}

/// Names whose meaning depends on the engine.
fn family_specific(family: TypeFamily, name: &TypeName) -> Option<LocalType> {
    let base = name.base.as_str();
    let mapped = match (family, base) {
        // SQL Server: TINYINT is 0–255, BIT is the boolean, TIMESTAMP is
        // ROWVERSION (eight opaque bytes), FLOAT(n) is single below 25 bits.
        (TypeFamily::SqlServer, "tinyint") => LocalType::UTinyInt,
        (TypeFamily::SqlServer, "bit") => LocalType::Boolean,
        (TypeFamily::SqlServer, "timestamp" | "rowversion") => LocalType::Blob,
        (TypeFamily::SqlServer, "money") => LocalType::Decimal {
            precision: 19,
            scale: 4,
        },
        (TypeFamily::SqlServer, "smallmoney") => LocalType::Decimal {
            precision: 10,
            scale: 4,
        },
        (TypeFamily::SqlServer | TypeFamily::Postgres, "float") => float_bits(&name.args, 53),
        // MySQL: FLOAT is single precision unless FLOAT(p > 24), REAL is
        // DOUBLE, BIT(n) is a bit string.
        (TypeFamily::MySql, "float") => float_bits(&name.args, 24),
        (TypeFamily::MySql, "real") => LocalType::Double,
        (TypeFamily::MySql, "bit") => LocalType::Blob,
        // Postgres money renders with the currency symbol.
        (TypeFamily::Postgres, "money") => LocalType::Varchar,
        (TypeFamily::Postgres, "bit" | "bit varying" | "varbit") => LocalType::Varchar,
        (TypeFamily::Postgres, "oid" | "xid" | "cid" | "regclass" | "regtype") => {
            LocalType::UInteger
        }
        // Oracle: DATE carries a time of day, FLOAT is binary precision.
        (TypeFamily::Oracle, "date") => LocalType::Timestamp,
        (TypeFamily::Oracle, "float") => float_bits(&name.args, 126),
        (TypeFamily::Oracle, "long") => LocalType::Varchar,
        (TypeFamily::Oracle, "rowid" | "urowid") => LocalType::Varchar,
        // DuckDB renders nested values as JSON text.
        (TypeFamily::DuckDb, "list" | "array" | "struct" | "map" | "union") => LocalType::Json,
        (TypeFamily::DuckDb, "timestamp_s" | "timestamp_ms" | "timestamp_ns") => {
            LocalType::Timestamp
        }
        (TypeFamily::ClickHouse, _) => return clickhouse_type(name),
        _ => return None,
    };
    Some(mapped)
}

/// FLOAT(n): `n` binary digits, single precision up to 24. `default_bits`
/// applies when no `n` is given.
fn float_bits(args: &[String], default_bits: u32) -> LocalType {
    if numeric_arg(args, 0).unwrap_or(default_bits) <= 24 {
        LocalType::Float
    } else {
        LocalType::Double
    }
}

fn clickhouse_type(name: &TypeName) -> Option<LocalType> {
    let mapped = match name.base.as_str() {
        "int8" => LocalType::TinyInt,
        "int16" => LocalType::SmallInt,
        "int32" => LocalType::Integer,
        "int64" => LocalType::BigInt,
        "int128" => LocalType::HugeInt,
        "uint8" => LocalType::UTinyInt,
        "uint16" => LocalType::USmallInt,
        "uint32" => LocalType::UInteger,
        "uint64" => LocalType::UBigInt,
        "uint128" => LocalType::UHugeInt,
        "int256" | "uint256" => LocalType::Varchar,
        "float32" => LocalType::Float,
        "float64" => LocalType::Double,
        "decimal32" => scaled_decimal(9, &name.args),
        "decimal64" => scaled_decimal(18, &name.args),
        "decimal128" => scaled_decimal(38, &name.args),
        "decimal256" => LocalType::Varchar,
        "string" | "fixedstring" => LocalType::Varchar,
        "date" | "date32" => LocalType::Date,
        "datetime" | "datetime64" => LocalType::Timestamp,
        "bool" | "boolean" => LocalType::Boolean,
        "uuid" => LocalType::Uuid,
        "json" | "object" => LocalType::Json,
        "decimal" => decimal_from_args(&name.args),
        _ => LocalType::Varchar,
    };
    Some(mapped)
}

/// ClickHouse's `DecimalNN(s)`.
fn scaled_decimal(precision: u32, args: &[String]) -> LocalType {
    match numeric_arg(args, 0) {
        Some(scale) => LocalType::decimal(precision, scale),
        None => LocalType::InferDecimal,
    }
}

/// The names engines share (ANSI, plus the common vendor spellings).
fn common_type(family: TypeFamily, name: &TypeName) -> LocalType {
    let unsigned = name.unsigned;
    let pick = |signed: LocalType, unsigned_type: LocalType| {
        if unsigned { unsigned_type } else { signed }
    };
    let base = name.base.as_str();
    match base {
        "bool" | "boolean" | "bit" => LocalType::Boolean,
        "tinyint" | "int1" | "byteint" => pick(LocalType::TinyInt, LocalType::UTinyInt),
        "smallint" | "int2" | "smallserial" | "serial2" => {
            pick(LocalType::SmallInt, LocalType::USmallInt)
        }
        "mediumint" | "int3" => pick(LocalType::Integer, LocalType::UInteger),
        "int" | "integer" | "int4" | "serial" | "serial4" | "int32" => {
            pick(LocalType::Integer, LocalType::UInteger)
        }
        "bigint" | "int8" | "bigserial" | "serial8" | "int64" => {
            pick(LocalType::BigInt, LocalType::UBigInt)
        }
        "utinyint" => LocalType::UTinyInt,
        "usmallint" => LocalType::USmallInt,
        "uinteger" => LocalType::UInteger,
        "ubigint" => LocalType::UBigInt,
        "hugeint" | "int128" => LocalType::HugeInt,
        "uhugeint" | "uint128" => LocalType::UHugeInt,
        "real" | "float4" | "float32" | "binary_float" => LocalType::Float,
        "float" | "double" | "double precision" | "float8" | "float64" | "binary_double" => {
            LocalType::Double
        }
        "decimal" | "numeric" | "dec" | "number" | "bignumeric" | "bigdecimal" => {
            decimal_from_args(&name.args)
        }
        "money" => LocalType::Decimal {
            precision: 19,
            scale: 4,
        },
        "char"
        | "character"
        | "nchar"
        | "national character"
        | "varchar"
        | "character varying"
        | "nvarchar"
        | "national character varying"
        | "varchar2"
        | "nvarchar2"
        | "text"
        | "ntext"
        | "tinytext"
        | "mediumtext"
        | "longtext"
        | "string"
        | "bpchar"
        | "name"
        | "citext"
        | "clob"
        | "nclob"
        | "dbclob"
        | "graphic"
        | "vargraphic" => LocalType::Varchar,
        "uuid" | "uniqueidentifier" | "guid" => LocalType::Uuid,
        "json" | "jsonb" => LocalType::Json,
        "date" | "date32" => LocalType::Date,
        "time" | "time without time zone" => LocalType::Time,
        "timetz" | "time with time zone" => LocalType::TimeTz,
        "timestamp"
        | "timestamp without time zone"
        | "datetime"
        | "datetime2"
        | "smalldatetime"
        | "timestamp_ntz"
        | "datetime64" => LocalType::Timestamp,
        "timestamptz"
        | "timestamp with time zone"
        | "datetimeoffset"
        | "timestamp_tz"
        | "timestamp_ltz"
        | "timestamp with local time zone" => LocalType::TimestampTz,
        "blob" | "bytea" | "binary" | "varbinary" | "image" | "raw" | "long raw" | "tinyblob"
        | "mediumblob" | "longblob" | "bytes" | "binary varying" | "varbyte" => LocalType::Blob,
        "year" => LocalType::SmallInt,
        _ if base.starts_with("interval") => LocalType::Interval,
        _ if family == TypeFamily::DuckDb && base.starts_with("decimal") => {
            decimal_from_args(&name.args)
        }
        _ => LocalType::Varchar,
    }
}

/// SQLite's declared types follow its column-affinity rules (section 3.1 of
/// the datatype docs), with the common names that carry more than affinity
/// (BOOLEAN, DATE, DECIMAL(p,s), …) honored first.
fn sqlite_type(declared: &str) -> LocalType {
    let name = parse(declared);
    let base = name.base.as_str();
    if base.is_empty() {
        return LocalType::Infer;
    }
    match base {
        "boolean" | "bool" => return LocalType::Boolean,
        "date" => return LocalType::Date,
        "datetime" | "timestamp" => return LocalType::Timestamp,
        "time" => return LocalType::Time,
        "json" => return LocalType::Json,
        "uuid" => return LocalType::Uuid,
        "decimal" | "numeric" => return decimal_from_args(&name.args),
        _ => {}
    }
    if base.contains("int") {
        LocalType::BigInt
    } else if base.contains("char") || base.contains("clob") || base.contains("text") {
        LocalType::Varchar
    } else if base.contains("blob") {
        LocalType::Blob
    } else if base.contains("real") || base.contains("floa") || base.contains("doub") {
        LocalType::Double
    } else {
        LocalType::Infer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DatabaseType as D;
    use LocalType as L;

    fn dec(precision: u8, scale: u8) -> LocalType {
        L::Decimal { precision, scale }
    }

    fn check(db: DatabaseType, cases: &[(&str, LocalType)]) {
        for (source, expected) in cases {
            assert_eq!(
                local_type_for(db, source),
                *expected,
                "{db:?} {source:?} should map to {expected:?}"
            );
        }
    }

    #[test]
    fn parse_splits_words_arguments_and_unsigned() {
        assert_eq!(
            parse("TIMESTAMP(6) WITH TIME ZONE"),
            TypeName {
                base: "timestamp with time zone".into(),
                args: vec!["6".into()],
                unsigned: false,
            }
        );
        assert_eq!(
            parse("decimal(10, 2) unsigned zerofill"),
            TypeName {
                base: "decimal".into(),
                args: vec!["10".into(), "2".into()],
                unsigned: true,
            }
        );
        assert_eq!(parse("Nullable(LowCardinality(String))").base, "string");
        assert_eq!(
            parse("Map(String, Array(UInt8))").args,
            vec!["string".to_string(), "array(uint8)".to_string()]
        );
        assert_eq!(
            parse("INTERVAL DAY(2) TO SECOND(6)").base,
            "interval day to second"
        );
    }

    #[test]
    fn sql_server_types() {
        check(
            D::SqlServer,
            &[
                ("bit", L::Boolean),
                ("tinyint", L::UTinyInt),
                ("smallint", L::SmallInt),
                ("int", L::Integer),
                ("bigint", L::BigInt),
                ("real", L::Float),
                ("float", L::Double),
                ("float(24)", L::Float),
                ("money", dec(19, 4)),
                ("smallmoney", dec(10, 4)),
                ("decimal", L::InferDecimal),
                ("numeric", L::InferDecimal),
                ("decimal(18,2)", dec(18, 2)),
                ("numeric(38, 10)", dec(38, 10)),
                ("datetime", L::Timestamp),
                ("datetime2", L::Timestamp),
                ("smalldatetime", L::Timestamp),
                ("datetimeoffset", L::TimestampTz),
                ("date", L::Date),
                ("time", L::Time),
                ("uniqueidentifier", L::Uuid),
                ("varbinary", L::Blob),
                ("image", L::Blob),
                ("timestamp", L::Blob),
                ("varchar", L::Varchar),
                ("nvarchar", L::Varchar),
                ("ntext", L::Varchar),
                ("xml", L::Varchar),
                ("sql_variant", L::Varchar),
                ("udt", L::Varchar),
                ("hierarchyid", L::Varchar),
                ("null", L::Varchar),
            ],
        );
        assert_eq!(
            local_type_for(D::AzureSqlDatabase, "money"),
            local_type_for(D::SqlServer, "money")
        );
    }

    #[test]
    fn postgres_types() {
        check(
            D::Postgres,
            &[
                ("bool", L::Boolean),
                ("int2", L::SmallInt),
                ("int4", L::Integer),
                ("int8", L::BigInt),
                ("float4", L::Float),
                ("float8", L::Double),
                ("double precision", L::Double),
                ("numeric", L::InferDecimal),
                ("numeric(12,3)", dec(12, 3)),
                ("numeric(1000,2)", L::Varchar),
                ("money", L::Varchar),
                ("text", L::Varchar),
                ("varchar", L::Varchar),
                ("bpchar", L::Varchar),
                ("name", L::Varchar),
                ("bytea", L::Blob),
                ("date", L::Date),
                ("time", L::Time),
                ("timetz", L::TimeTz),
                ("timestamp", L::Timestamp),
                ("timestamptz", L::TimestampTz),
                ("interval", L::Interval),
                ("uuid", L::Uuid),
                ("json", L::Json),
                ("jsonb", L::Json),
                ("oid", L::UInteger),
                ("inet", L::Varchar),
                ("_int4", L::Varchar),
                ("integer[]", L::Varchar),
                ("varbit", L::Varchar),
                ("tsvector", L::Varchar),
            ],
        );
        for db in [
            D::CockroachDb,
            D::Neon,
            D::Supabase,
            D::AmazonAuroraPostgres,
        ] {
            assert_eq!(local_type_for(db, "timestamptz"), L::TimestampTz);
        }
    }

    #[test]
    fn mysql_types() {
        check(
            D::MySql,
            &[
                ("tinyint", L::TinyInt),
                ("tinyint unsigned", L::UTinyInt),
                ("smallint unsigned", L::USmallInt),
                ("mediumint", L::Integer),
                ("mediumint unsigned", L::UInteger),
                ("int", L::Integer),
                ("int unsigned", L::UInteger),
                ("bigint", L::BigInt),
                ("bigint unsigned", L::UBigInt),
                ("float", L::Float),
                ("double", L::Double),
                ("real", L::Double),
                ("decimal", L::InferDecimal),
                ("decimal(10,2)", dec(10, 2)),
                ("year", L::SmallInt),
                ("bit", L::Blob),
                ("date", L::Date),
                ("time", L::Time),
                ("datetime", L::Timestamp),
                ("timestamp", L::Timestamp),
                ("json", L::Json),
                ("enum", L::Varchar),
                ("set", L::Varchar),
                ("geometry", L::Varchar),
                ("vector", L::Varchar),
                ("blob", L::Blob),
                ("varbinary", L::Blob),
                ("binary", L::Blob),
                ("text", L::Varchar),
                ("char", L::Varchar),
                ("varchar", L::Varchar),
                ("null", L::Varchar),
            ],
        );
        for db in [D::MariaDb, D::TiDb, D::PlanetScale, D::AmazonAurora] {
            assert_eq!(local_type_for(db, "bigint unsigned"), L::UBigInt);
        }
    }

    #[test]
    fn sqlite_types_follow_affinity() {
        check(
            D::Sqlite,
            &[
                ("", L::Infer),
                ("integer", L::BigInt),
                ("int", L::BigInt),
                ("unsigned big int", L::BigInt),
                ("varchar(255)", L::Varchar),
                ("nvarchar(10)", L::Varchar),
                ("text", L::Varchar),
                ("clob", L::Varchar),
                ("blob", L::Blob),
                ("real", L::Double),
                ("double precision", L::Double),
                ("float", L::Double),
                ("numeric", L::InferDecimal),
                ("decimal(10,5)", dec(10, 5)),
                ("boolean", L::Boolean),
                ("date", L::Date),
                ("datetime", L::Timestamp),
                ("json", L::Json),
                ("something_custom", L::Infer),
            ],
        );
        assert_eq!(local_type_for(D::LibSql, "integer"), L::BigInt);
    }

    #[test]
    fn oracle_types() {
        check(
            D::Oracle,
            &[
                ("number", L::InferDecimal),
                ("number(10)", dec(10, 0)),
                ("number(10,2)", dec(10, 2)),
                ("float", L::Double),
                ("float(20)", L::Float),
                ("binary_float", L::Float),
                ("binary_double", L::Double),
                ("varchar2(20)", L::Varchar),
                ("nvarchar2(20)", L::Varchar),
                ("char(1)", L::Varchar),
                ("clob", L::Varchar),
                ("nclob", L::Varchar),
                ("long", L::Varchar),
                ("raw(16)", L::Blob),
                ("long raw", L::Blob),
                ("blob", L::Blob),
                ("date", L::Timestamp),
                ("timestamp", L::Timestamp),
                ("timestamp(3)", L::Timestamp),
                ("timestamp with time zone", L::TimestampTz),
                ("timestamp(9) with local time zone", L::TimestampTz),
                ("interval day to second", L::Interval),
                ("interval year(4) to month", L::Interval),
                ("rowid", L::Varchar),
                ("boolean", L::Boolean),
                ("json", L::Json),
                ("xml", L::Varchar),
                ("hr.address_t", L::Varchar),
            ],
        );
    }

    #[test]
    fn duckdb_types_map_to_themselves() {
        check(
            D::DuckDb,
            &[
                ("boolean", L::Boolean),
                ("tinyint", L::TinyInt),
                ("utinyint", L::UTinyInt),
                ("usmallint", L::USmallInt),
                ("uinteger", L::UInteger),
                ("ubigint", L::UBigInt),
                ("hugeint", L::HugeInt),
                ("uhugeint", L::UHugeInt),
                ("float", L::Double),
                ("double", L::Double),
                ("decimal(18,3)", dec(18, 3)),
                ("varchar", L::Varchar),
                ("blob", L::Blob),
                ("date", L::Date),
                ("time", L::Time),
                ("time with time zone", L::TimeTz),
                ("timestamp", L::Timestamp),
                ("timestamp_ns", L::Timestamp),
                ("timestamp with time zone", L::TimestampTz),
                ("interval", L::Interval),
                ("uuid", L::Uuid),
                ("json", L::Json),
                ("list", L::Json),
                ("struct", L::Json),
                ("map", L::Json),
                ("enum", L::Varchar),
                ("bignum", L::Varchar),
                ("unknown", L::Varchar),
            ],
        );
    }

    #[test]
    fn clickhouse_types() {
        check(
            D::ClickHouse,
            &[
                ("Int8", L::TinyInt),
                ("Int16", L::SmallInt),
                ("Int32", L::Integer),
                ("Int64", L::BigInt),
                ("Int128", L::HugeInt),
                ("Int256", L::Varchar),
                ("UInt8", L::UTinyInt),
                ("UInt64", L::UBigInt),
                ("UInt128", L::UHugeInt),
                ("Float32", L::Float),
                ("Float64", L::Double),
                ("Decimal(18, 4)", dec(18, 4)),
                ("Decimal32(2)", dec(9, 2)),
                ("Decimal64(4)", dec(18, 4)),
                ("Decimal128(10)", dec(38, 10)),
                ("Decimal256(10)", L::Varchar),
                ("String", L::Varchar),
                ("FixedString(16)", L::Varchar),
                ("Nullable(String)", L::Varchar),
                ("LowCardinality(Nullable(String))", L::Varchar),
                ("Nullable(Int64)", L::BigInt),
                ("Date", L::Date),
                ("Date32", L::Date),
                ("DateTime", L::Timestamp),
                ("DateTime64(3, 'UTC')", L::Timestamp),
                ("Bool", L::Boolean),
                ("UUID", L::Uuid),
                ("Array(String)", L::Varchar),
                ("Map(String, UInt64)", L::Varchar),
                ("Enum8('a' = 1, 'b' = 2)", L::Varchar),
                ("IPv4", L::Varchar),
            ],
        );
    }

    #[test]
    fn generic_engines_use_the_common_names() {
        check(
            D::Snowflake,
            &[
                ("NUMBER(38,0)", dec(38, 0)),
                ("NUMBER", L::InferDecimal),
                ("FLOAT", L::Double),
                ("VARCHAR(16777216)", L::Varchar),
                ("BOOLEAN", L::Boolean),
                ("TIMESTAMP_NTZ", L::Timestamp),
                ("TIMESTAMP_TZ", L::TimestampTz),
                ("TIMESTAMP_LTZ", L::TimestampTz),
                ("VARIANT", L::Varchar),
                ("BINARY", L::Blob),
            ],
        );
        check(
            D::Spanner,
            &[
                ("INT64", L::BigInt),
                ("FLOAT64", L::Double),
                ("STRING", L::Varchar),
                ("BYTES", L::Blob),
                ("NUMERIC", L::InferDecimal),
                ("BOOL", L::Boolean),
            ],
        );
        check(
            D::Databricks,
            &[
                ("INT", L::Integer),
                ("BIGINT", L::BigInt),
                ("DECIMAL(10,2)", dec(10, 2)),
                ("STRING", L::Varchar),
                ("TIMESTAMP", L::Timestamp),
            ],
        );
        check(D::Redis, &[("string", L::Varchar), ("integer", L::Integer)]);
    }

    #[test]
    fn every_engine_has_a_family_and_unknown_names_become_varchar() {
        for db in DatabaseType::ALL {
            let family = TypeFamily::of(db);
            let fallback = local_type_for(db, "definitely_not_a_type");
            if family == TypeFamily::Sqlite {
                assert_eq!(fallback, L::Infer);
            } else {
                assert_eq!(fallback, L::Varchar, "{db:?}");
            }
            assert_eq!(local_type_for(db, "   ").sql(), "VARCHAR");
        }
    }

    #[test]
    fn decimals_out_of_duckdb_range_stay_exact_as_text() {
        assert_eq!(local_type_for(D::Postgres, "numeric(39,2)"), L::Varchar);
        assert_eq!(local_type_for(D::Postgres, "numeric(5,6)"), L::Varchar);
        assert_eq!(local_type_for(D::Postgres, "numeric(0)"), L::Varchar);
        assert_eq!(dec(38, 38).sql(), "DECIMAL(38,38)");
    }
}
