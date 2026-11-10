//! Data types: native dialect spelling + canonical type class (ADR-011 §1.2).
//! The canonical class is what cross-dialect validation (ADR-010 slot rules)
//! consumes; the native spelling is preserved for rendering and
//! dialect-specific refinements.

use crate::sqlmodel::{Dialect, ObjectRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeClass {
    Integer,
    Decimal,
    Float,
    Character,
    Binary,
    DateTime,
    Boolean,
    Uuid,
    Json,
    Xml,
    Spatial,
    RowId,
    Interval,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeParams {
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    /// Length in characters/bytes; `None` for unbounded (max/TEXT).
    pub length: Option<u32>,
    pub with_timezone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeRef {
    /// Exact dialect spelling, e.g. "NUMBER(10,2)", "nvarchar(max)", "TEXT".
    pub native: String,
    pub class: TypeClass,
    pub params: TypeParams,
    /// Domain/alias/UDT this resolves through, if any.
    pub custom: Option<ObjectRef>,
}

impl TypeRef {
    /// Parse a native type spelling into a TypeRef. Unknown names map to
    /// `TypeClass::Other` with `custom: None`; resolution against user-defined
    /// types happens later at model scope.
    pub fn parse(native: &str, dialect: Dialect) -> TypeRef {
        let trimmed = native.trim();
        let (base, params) = split_params(trimmed);
        let class = classify(&base, &params, dialect);
        TypeRef {
            native: trimmed.to_string(),
            class,
            params,
            custom: None,
        }
    }

    /// Integer range for range checking, where the type has a defined one.
    ///
    /// When the dialect is unknown, callers should use
    /// [`integer_range_for`](Self::integer_range_for) with the active dialect.
    /// This method keeps the conservative SQL Server unsigned `tinyint` range.
    pub fn integer_range(&self) -> Option<(i128, i128)> {
        self.integer_range_for(Dialect::SqlServer)
    }

    /// Dialect-aware integer range for range checking.
    pub fn integer_range_for(&self, dialect: Dialect) -> Option<(i128, i128)> {
        let base = base_name(&self.native);
        let unsigned = self.native.to_ascii_lowercase().contains("unsigned");
        match base.as_str() {
            "bit" => Some((0, 1)),
            "tinyint" => {
                if unsigned {
                    Some((0, 255))
                } else if matches!(dialect, Dialect::MySql) {
                    Some((i8::MIN as i128, i8::MAX as i128))
                } else {
                    Some((0, 255))
                }
            }
            "smallint" | "int2" => Some((i16::MIN as i128, i16::MAX as i128)),
            "int" | "integer" | "int4" | "mediumint" => Some((i32::MIN as i128, i32::MAX as i128)),
            "bigint" | "int8" => Some((i64::MIN as i128, i64::MAX as i128)),
            _ => None,
        }
    }
}

fn base_name(native: &str) -> String {
    let lower = native.trim().to_lowercase();
    lower.split(['(', ' ']).next().unwrap_or("").to_string()
}

fn split_params(native: &str) -> (String, TypeParams) {
    let lower = native.to_lowercase();
    let mut params = TypeParams::default();
    if lower.contains("with time zone") || lower.ends_with("tz") {
        params.with_timezone = true;
    }
    let base = base_name(native);
    if let (Some(open), Some(close)) = (lower.find('('), lower.rfind(')'))
        && open < close
    {
        let inner = &lower[open + 1..close];
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        match parts.as_slice() {
            ["max"] => {}
            [a] => {
                if let Ok(n) = a.parse::<u32>() {
                    params.length = Some(n);
                    params.precision = Some(n);
                }
            }
            [a, b] => {
                params.precision = a.parse().ok();
                params.scale = b.parse().ok();
            }
            _ => {}
        }
    }
    (base, params)
}

fn classify(base: &str, params: &TypeParams, dialect: Dialect) -> TypeClass {
    use TypeClass::*;
    match base {
        // Integer family. SQL Server `bit` is integer-ish 0/1 (ADR-010 treats
        // it as integer-like); Postgres/MySQL `bit` is a bit string → Binary.
        "bit" => match dialect {
            Dialect::SqlServer => Integer,
            _ => Binary,
        },
        "tinyint" | "smallint" | "int" | "integer" | "bigint" | "mediumint" | "int2" | "int4"
        | "int8" | "serial" | "bigserial" | "smallserial" | "pls_integer" | "binary_integer" => {
            Integer
        }
        // Oracle NUMBER: scale 0 (or no params at all with precision set) is
        // integer-like; otherwise decimal.
        "number" => {
            if params.scale.unwrap_or(0) == 0 && params.precision.is_some() {
                Integer
            } else {
                Decimal
            }
        }
        "decimal" | "numeric" | "money" | "smallmoney" | "dec" => Decimal,
        "float" | "real" | "double" | "binary_float" | "binary_double" | "float4" | "float8" => {
            Float
        }
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "tinytext"
        | "mediumtext" | "longtext" | "varchar2" | "nvarchar2" | "clob" | "nclob" | "long"
        | "character" | "charactervarying" | "citext" | "enum" | "set" => Character,
        "binary" | "varbinary" | "image" | "blob" | "tinyblob" | "mediumblob" | "longblob"
        | "bytea" | "raw" | "longraw" => Binary,
        "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time"
        | "timestamp" | "timestamptz" | "timetz" | "year" => DateTime,
        "boolean" | "bool" => Boolean,
        "uniqueidentifier" | "uuid" => Uuid,
        "json" | "jsonb" => Json,
        "xml" | "xmltype" => Xml,
        "geometry" | "geography" | "point" | "polygon" | "linestring" | "sdo_geometry" => Spatial,
        "rowid" | "urowid" | "rowversion" => RowId,
        "interval" => Interval,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_across_dialects() {
        assert_eq!(
            TypeRef::parse("nvarchar(max)", Dialect::SqlServer).class,
            TypeClass::Character
        );
        assert_eq!(
            TypeRef::parse("NUMBER(10,2)", Dialect::Oracle).class,
            TypeClass::Decimal
        );
        assert_eq!(
            TypeRef::parse("NUMBER(10)", Dialect::Oracle).class,
            TypeClass::Integer
        );
        assert_eq!(
            TypeRef::parse("bit", Dialect::SqlServer).class,
            TypeClass::Integer
        );
        assert_eq!(
            TypeRef::parse("bit", Dialect::Postgres).class,
            TypeClass::Binary
        );
        assert_eq!(
            TypeRef::parse("uniqueidentifier", Dialect::SqlServer).class,
            TypeClass::Uuid
        );
        assert_eq!(
            TypeRef::parse("uuid", Dialect::Postgres).class,
            TypeClass::Uuid
        );
        assert_eq!(
            TypeRef::parse("TEXT", Dialect::Sqlite).class,
            TypeClass::Character
        );
        assert_eq!(
            TypeRef::parse("jsonb", Dialect::Postgres).class,
            TypeClass::Json
        );
        assert!(
            TypeRef::parse("timestamp with time zone", Dialect::Postgres)
                .params
                .with_timezone
        );
        assert_eq!(
            TypeRef::parse("MyCustomType", Dialect::SqlServer).class,
            TypeClass::Other
        );
    }

    #[test]
    fn params_parsed() {
        let t = TypeRef::parse("decimal(18, 4)", Dialect::SqlServer);
        assert_eq!(t.params.precision, Some(18));
        assert_eq!(t.params.scale, Some(4));
        let v = TypeRef::parse("varchar(50)", Dialect::MySql);
        assert_eq!(v.params.length, Some(50));
    }

    #[test]
    fn integer_ranges() {
        assert_eq!(
            TypeRef::parse("tinyint", Dialect::SqlServer).integer_range_for(Dialect::SqlServer),
            Some((0, 255))
        );
        assert_eq!(
            TypeRef::parse("tinyint", Dialect::MySql).integer_range_for(Dialect::MySql),
            Some((i8::MIN as i128, i8::MAX as i128))
        );
        assert_eq!(
            TypeRef::parse("tinyint unsigned", Dialect::MySql).integer_range_for(Dialect::MySql),
            Some((0, 255))
        );
        assert_eq!(
            TypeRef::parse("int", Dialect::SqlServer).integer_range(),
            Some((i32::MIN as i128, i32::MAX as i128))
        );
        assert_eq!(
            TypeRef::parse("varchar(10)", Dialect::SqlServer).integer_range(),
            None
        );
    }
}
