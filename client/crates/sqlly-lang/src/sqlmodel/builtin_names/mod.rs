//! The names of each dialect's built-in (system) functions and data types,
//! so the formatter can tell `GETDATE()` from a user's `dbo.fnTotals()` and
//! `nvarchar` from a user's `dbo.OrderIdList`, and only ever re-case the
//! system ones.
//!
//! `data.rs` is generated from the IntelliSense builtin catalog
//! (`sqlly-intel/data/builtins/*.json`), which this crate cannot depend on
//! and which is far too large to embed in the online formatter's wasm. A
//! sqlly-intel test fails when the two drift; regenerate with
//! `SQLLY_REGEN_BUILTIN_NAMES=1 cargo test -p sqlly-intel builtin_names_match_catalog`.

mod data;
mod warehouse;

use super::Dialect;

const DIALECTS: [Dialect; 5] = [
    Dialect::SqlServer,
    Dialect::Postgres,
    Dialect::MySql,
    Dialect::Oracle,
    Dialect::Sqlite,
];

/// Every built-in function name of `dialect` (common entries included),
/// ASCII-uppercased and sorted. `@@` globals are left out: they are never
/// called with parentheses.
pub fn builtin_function_names(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => data::SQLSERVER_FUNCTIONS,
        Dialect::Postgres => data::POSTGRES_FUNCTIONS,
        Dialect::MySql => data::MYSQL_FUNCTIONS,
        Dialect::Oracle => data::ORACLE_FUNCTIONS,
        Dialect::Sqlite => data::SQLITE_FUNCTIONS,
    }
}

/// Every built-in data type name of `dialect`, ASCII-uppercased and sorted.
pub fn builtin_type_names(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => data::SQLSERVER_TYPES,
        Dialect::Postgres => data::POSTGRES_TYPES,
        Dialect::MySql => data::MYSQL_TYPES,
        Dialect::Oracle => data::ORACLE_TYPES,
        Dialect::Sqlite => data::SQLITE_TYPES,
    }
}

fn contains(names: &[&str], name: &str) -> bool {
    names
        .binary_search(&name.to_ascii_uppercase().as_str())
        .is_ok()
}

/// Whether `name` is a built-in function of `dialect`. Case-insensitive.
pub fn is_builtin_function(dialect: Dialect, name: &str) -> bool {
    contains(builtin_function_names(dialect), name)
}

/// Whether `name` is a built-in function of any supported dialect, or of
/// BigQuery, Snowflake or Redshift, for input whose engine has no catalog
/// of its own or is only a guess. Case-insensitive.
pub fn is_builtin_function_in_any_dialect(name: &str) -> bool {
    DIALECTS.into_iter().any(|d| is_builtin_function(d, name))
        || contains(warehouse::WAREHOUSE_FUNCTIONS, name)
}

/// Whether `name` is a built-in data type of `dialect`. Case-insensitive.
pub fn is_builtin_type(dialect: Dialect, name: &str) -> bool {
    contains(builtin_type_names(dialect), name)
}

/// Whether `name` is a built-in data type of any supported dialect.
/// Case-insensitive.
pub fn is_builtin_type_in_any_dialect(name: &str) -> bool {
    DIALECTS.into_iter().any(|d| is_builtin_type(d, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_uppercase_and_unique() {
        for d in DIALECTS {
            for names in [
                builtin_function_names(d),
                builtin_type_names(d),
                warehouse::WAREHOUSE_FUNCTIONS,
            ] {
                assert!(!names.is_empty(), "{d:?}");
                assert!(
                    names.windows(2).all(|w| w[0] < w[1]),
                    "{d:?} not sorted/unique"
                );
                assert!(names.iter().all(|n| *n == n.to_ascii_uppercase()), "{d:?}");
            }
        }
    }

    #[test]
    fn lookups_are_case_insensitive_and_dialect_specific() {
        assert!(is_builtin_function(Dialect::SqlServer, "getdate"));
        assert!(is_builtin_function(Dialect::SqlServer, "GetDate"));
        assert!(is_builtin_function(Dialect::Postgres, "date_trunc"));
        assert!(!is_builtin_function(Dialect::SqlServer, "date_trunc"));
        assert!(is_builtin_function_in_any_dialect("date_trunc"));
        assert!(!is_builtin_function_in_any_dialect("fnGetCustomerTotals"));
        assert!(is_builtin_function_in_any_dialect("timestamp_sub"));
        assert!(is_builtin_function_in_any_dialect("IFF"));
        assert!(is_builtin_type(Dialect::SqlServer, "Hierarchyid"));
        assert!(is_builtin_type(Dialect::Postgres, "jsonb"));
        assert!(!is_builtin_type_in_any_dialect("OrderIdList"));
    }
}
