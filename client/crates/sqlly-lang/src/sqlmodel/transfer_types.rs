//! Cross-engine column type mapping for data transfer.
//!
//! Pure classification and DDL spelling — no I/O.

use crate::sqlmodel::{
    DatabaseType, Dialect, LogicalType, TypeClass, TypeMapping, TypeParams, TypeRef, TypeStorage,
};

/// Why a mapping is not exact. Surfaced verbatim in the wizard's summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MappingNote {
    Widened,
    Narrowed,
    Encoded(TypeStorage),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedColumnType {
    pub logical: LogicalType,
    /// Full target DDL spelling, e.g. `nvarchar(120)`, `decimal(18,2)`, `bigint`.
    pub target_ddl: String,
    pub note: Option<MappingNote>,
}

fn base_name(native: &str) -> String {
    let lower = native.trim().to_lowercase();
    lower.split(['(', ' ']).next().unwrap_or("").to_string()
}

fn is_unbounded_character(native: &str, params: &TypeParams) -> bool {
    let lower = native.trim().to_lowercase();
    if lower.contains("(max)") {
        return true;
    }
    matches!(
        base_name(native).as_str(),
        "text" | "ntext" | "longtext" | "mediumtext" | "tinytext" | "clob" | "nclob" | "long"
    ) || (params.length.is_none()
        && matches!(
            base_name(native).as_str(),
            "text" | "longtext" | "clob" | "ntext"
        ))
}

/// Classify a source engine's native type spelling into the logical type used for DDL on another engine.
pub fn logical_type_of(
    native: &str,
    source: DatabaseType,
) -> (LogicalType, TypeParams, Option<MappingNote>) {
    let parsed = TypeRef::parse(native, source.family());
    let base = base_name(native);
    let params = parsed.params;

    match parsed.class {
        TypeClass::Integer => classify_integer(&base, native, source.family(), params),
        TypeClass::Decimal => {
            if matches!(base.as_str(), "money" | "smallmoney") {
                (
                    LogicalType::Decimal,
                    TypeParams {
                        precision: Some(19),
                        scale: Some(4),
                        ..params
                    },
                    Some(MappingNote::Widened),
                )
            } else {
                (LogicalType::Decimal, params, None)
            }
        }
        TypeClass::Float => {
            let note = if matches!(base.as_str(), "real" | "float4") {
                Some(MappingNote::Widened)
            } else {
                None
            };
            (LogicalType::Float, params, note)
        }
        TypeClass::Character => classify_character(native, &base, params),
        TypeClass::Binary => (LogicalType::Binary, params, None),
        TypeClass::DateTime => classify_datetime(native, &base, params),
        TypeClass::Boolean => (LogicalType::Boolean, params, None),
        TypeClass::Uuid => (LogicalType::Uuid, params, None),
        TypeClass::Json => (LogicalType::Json, params, None),
        TypeClass::Xml => (LogicalType::Xml, params, None),
        TypeClass::Spatial | TypeClass::RowId | TypeClass::Interval | TypeClass::Other => {
            (LogicalType::Text, params, Some(MappingNote::Unknown))
        }
    }
}

fn classify_integer(
    base: &str,
    native: &str,
    family: Dialect,
    params: TypeParams,
) -> (LogicalType, TypeParams, Option<MappingNote>) {
    let lower_native = native.to_ascii_lowercase();
    match base {
        "bigint" | "int8" | "serial8" | "bigserial" => (LogicalType::BigInt, params, None),
        "bit" if family == Dialect::SqlServer => (LogicalType::Boolean, params, None),
        "tinyint" if lower_native.contains("(1)") => (LogicalType::Boolean, params, None),
        "boolean" | "bool" => (LogicalType::Boolean, params, None),
        "int" | "integer" if lower_native.contains("unsigned") => {
            (LogicalType::BigInt, params, None)
        }
        _ => (LogicalType::Integer, params, None),
    }
}

fn classify_character(
    native: &str,
    base: &str,
    params: TypeParams,
) -> (LogicalType, TypeParams, Option<MappingNote>) {
    if base == "char" || base == "nchar" || base == "character" {
        return (LogicalType::VarChar, params, Some(MappingNote::Widened));
    }
    if is_unbounded_character(native, &params) {
        (LogicalType::Text, params, None)
    } else if params.length.is_some() {
        (LogicalType::VarChar, params, None)
    } else {
        (LogicalType::Text, params, None)
    }
}

fn classify_datetime(
    native: &str,
    base: &str,
    params: TypeParams,
) -> (LogicalType, TypeParams, Option<MappingNote>) {
    let lower = native.trim().to_lowercase();
    if base == "date" && !lower.starts_with("datetime") {
        return (LogicalType::Date, params, None);
    }
    if base.starts_with("time")
        && base != "datetime"
        && base != "datetime2"
        && base != "datetimeoffset"
    {
        return (LogicalType::Time, params, None);
    }
    if params.with_timezone || base == "datetimeoffset" || base == "timestamptz" {
        return (LogicalType::DateTimeOffset, params, None);
    }
    (LogicalType::DateTime, params, None)
}

fn clamp_varchar_length(target: DatabaseType, length: u32) -> u32 {
    use DatabaseType::*;
    match target {
        SqlServer | AzureSqlDatabase => length.min(4000),
        MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
        | PlanetScale | TiDb => length.min(65535),
        // VARCHAR2 tops out at 4000 bytes unless MAX_STRING_SIZE=EXTENDED.
        Oracle => length.min(4000),
        _ => length,
    }
}

/// The length a character column gets when the source declared none:
/// Oracle's `VARCHAR2` requires one (the widest standard size); every other
/// engine accepts the bare type.
fn default_varchar_length(target: DatabaseType) -> Option<u32> {
    match target {
        DatabaseType::Oracle => Some(4000),
        _ => None,
    }
}

fn format_target_ddl(
    target: DatabaseType,
    logical: LogicalType,
    mapping: &TypeMapping,
    params: &TypeParams,
) -> String {
    match logical {
        LogicalType::VarChar => {
            if let Some(len) = params.length.or_else(|| default_varchar_length(target)) {
                format!(
                    "{}({})",
                    mapping.native_name,
                    clamp_varchar_length(target, len)
                )
            } else {
                mapping.native_name.to_string()
            }
        }
        LogicalType::Decimal => {
            let (precision, scale) = if params.precision.is_some() || params.scale.is_some() {
                (params.precision.unwrap_or(18), params.scale.unwrap_or(0))
            } else if target != DatabaseType::Sqlite {
                (18, 4)
            } else {
                return mapping.native_name.to_string();
            };
            if target == DatabaseType::Sqlite {
                mapping.native_name.to_string()
            } else {
                format!("{}({},{})", mapping.native_name, precision, scale)
            }
        }
        _ => mapping.native_name.to_string(),
    }
}

/// The target column type for a source column: `map_type` for the base name plus carried length/precision/scale.
pub fn map_column_type(
    native: &str,
    source: DatabaseType,
    target: DatabaseType,
) -> MappedColumnType {
    let (logical, params, classifier_note) = logical_type_of(native, source);
    let mapping = target.map_type(logical);
    let target_ddl = format_target_ddl(target, logical, &mapping, &params);
    let note = if mapping.storage != TypeStorage::Native {
        Some(MappingNote::Encoded(mapping.storage))
    } else {
        classifier_note
    };
    MappedColumnType {
        logical,
        target_ddl,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlmodel::TypeStorage;

    const MATRIX_ENGINES: [DatabaseType; 6] = [
        DatabaseType::SqlServer,
        DatabaseType::AzureSqlDatabase,
        DatabaseType::Postgres,
        DatabaseType::MySql,
        DatabaseType::MariaDb,
        DatabaseType::Sqlite,
    ];

    const MATRIX_SOURCE_TYPES: [&str; 24] = [
        "int",
        "bigint",
        "bit",
        "tinyint(1)",
        "decimal(18,2)",
        "numeric(10)",
        "money",
        "float",
        "real",
        "nvarchar(120)",
        "varchar(255)",
        "char(3)",
        "nvarchar(max)",
        "text",
        "longtext",
        "varbinary(max)",
        "bytea",
        "blob",
        "date",
        "time",
        "datetime2",
        "timestamp with time zone",
        "uniqueidentifier",
        "jsonb",
    ];

    #[test]
    fn matrix_never_panics_and_emits_valid_ddl() {
        for source in MATRIX_ENGINES {
            for native in MATRIX_SOURCE_TYPES {
                for target in MATRIX_ENGINES {
                    let mapped = map_column_type(native, source, target);
                    assert!(
                        !mapped.target_ddl.is_empty(),
                        "{native} {source:?} -> {target:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn nvarchar_to_postgres_varchar() {
        let mapped = map_column_type(
            "nvarchar(120)",
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
        );
        assert_eq!(mapped.target_ddl, "varchar(120)");
    }

    #[test]
    fn bit_to_mysql_tinyint() {
        let mapped = map_column_type("bit", DatabaseType::SqlServer, DatabaseType::MySql);
        assert_eq!(mapped.target_ddl, "tinyint(1)");
        assert_eq!(
            mapped.note,
            Some(MappingNote::Encoded(TypeStorage::IntegerEncoded))
        );
    }

    #[test]
    fn jsonb_to_sql_server_nvarchar_max() {
        let mapped = map_column_type("jsonb", DatabaseType::Postgres, DatabaseType::SqlServer);
        assert_eq!(mapped.target_ddl, "nvarchar(max)");
        assert_eq!(
            mapped.note,
            Some(MappingNote::Encoded(TypeStorage::TextEncoded))
        );
    }

    #[test]
    fn oracle_varchar2_always_carries_a_length() {
        // A fixed-width character type with no declared length still needs
        // one on Oracle; a length-less `varchar` is unbounded text (CLOB).
        let mapped = map_column_type("nchar", DatabaseType::Sqlite, DatabaseType::Oracle);
        assert_eq!(mapped.target_ddl, "varchar2(4000)");
        let mapped = map_column_type("varchar", DatabaseType::Sqlite, DatabaseType::Oracle);
        assert_eq!(mapped.target_ddl, "clob");
        let mapped = map_column_type(
            "nvarchar(120)",
            DatabaseType::SqlServer,
            DatabaseType::Oracle,
        );
        assert_eq!(mapped.target_ddl, "varchar2(120)");
        let mapped = map_column_type("varchar(65535)", DatabaseType::MySql, DatabaseType::Oracle);
        assert_eq!(mapped.target_ddl, "varchar2(4000)");
        // Other engines keep the bare type when the source had no length.
        let mapped = map_column_type("nchar", DatabaseType::Sqlite, DatabaseType::Postgres);
        assert_eq!(mapped.target_ddl, "varchar");
    }

    #[test]
    fn datetime2_to_sqlite_text() {
        let mapped = map_column_type("datetime2", DatabaseType::SqlServer, DatabaseType::Sqlite);
        assert_eq!(mapped.target_ddl, "text");
    }

    #[test]
    fn geometry_to_mysql_unknown() {
        let mapped = map_column_type("geometry", DatabaseType::Postgres, DatabaseType::MySql);
        assert_eq!(mapped.note, Some(MappingNote::Unknown));
    }

    #[test]
    fn money_maps_to_decimal_with_scale() {
        let mapped = map_column_type("money", DatabaseType::SqlServer, DatabaseType::Postgres);
        assert_eq!(mapped.logical, LogicalType::Decimal);
        assert!(mapped.target_ddl.starts_with("decimal"));
    }
}
