//! Dialect and identifier concerns: which SQL dialects exist, how each folds
//! identifiers, dialect-aware names ([`SqlName`]), normalized object
//! references ([`ObjectRef`]), object kinds, and per-dialect capability flags.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Dialect
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Dialect {
    SqlServer,
    Postgres,
    MySql,
    Oracle, // parser family only; no native driver
    Sqlite,
}

impl Dialect {
    pub const ALL: [Dialect; 5] = [
        Dialect::SqlServer,
        Dialect::Postgres,
        Dialect::MySql,
        Dialect::Oracle,
        Dialect::Sqlite,
    ];

    /// Default identifier folding for unquoted identifiers.
    pub fn identifier_fold(self) -> IdentifierFold {
        match self {
            // SQL Server collation-dependent but case-insensitive by default.
            Dialect::SqlServer => IdentifierFold::CaseInsensitive,
            // Postgres folds unquoted identifiers to lowercase.
            Dialect::Postgres => IdentifierFold::Lower,
            // MySQL: databases/tables depend on filesystem; columns are
            // case-insensitive. We treat names case-insensitively and record
            // the per-source override at collection time.
            Dialect::MySql => IdentifierFold::CaseInsensitive,
            // Oracle folds unquoted identifiers to uppercase.
            Dialect::Oracle => IdentifierFold::Upper,
            Dialect::Sqlite => IdentifierFold::CaseInsensitive,
        }
    }

    /// Whether the dialect has a catalog level above namespaces.
    pub fn has_catalogs(self) -> bool {
        !matches!(self, Dialect::MySql)
    }

    /// Whether the dialect has namespaces (schemas) within a catalog.
    pub fn has_namespaces(self) -> bool {
        !matches!(self, Dialect::Sqlite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentifierFold {
    CaseInsensitive,
    Lower,
    Upper,
    Exact,
}

// ---------------------------------------------------------------------------
// Names and references
// ---------------------------------------------------------------------------

/// A name with its original spelling and a dialect-aware fold key.
/// Lookups use the fold key; rendering uses the original spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SqlName {
    spelling: String,
    fold: String,
}

impl SqlName {
    pub fn new(spelling: impl Into<String>, dialect: Dialect, quoted: bool) -> Self {
        let spelling = spelling.into();
        let fold = fold_identifier(&spelling, dialect, quoted);
        Self { spelling, fold }
    }

    pub fn spelling(&self) -> &str {
        &self.spelling
    }

    pub fn fold_key(&self) -> &str {
        &self.fold
    }

    pub fn matches(&self, other: &SqlName) -> bool {
        self.fold == other.fold
    }
}

pub fn fold_identifier(spelling: &str, dialect: Dialect, quoted: bool) -> String {
    match (dialect.identifier_fold(), quoted) {
        // Quoted identifiers preserve case in pg/oracle; the case-insensitive
        // dialects fold even quoted names for lookup purposes.
        (IdentifierFold::CaseInsensitive, _) => spelling.to_lowercase(),
        (IdentifierFold::Lower, false) => spelling.to_lowercase(),
        (IdentifierFold::Upper, false) => spelling.to_uppercase(),
        (IdentifierFold::Lower | IdentifierFold::Upper, true) => spelling.to_string(),
        (IdentifierFold::Exact, _) => spelling.to_string(),
    }
}

pub type CatalogName = SqlName;
pub type NamespaceName = SqlName;
pub type ObjectName = SqlName;

/// Normalized three-level object reference. Levels a dialect lacks are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub catalog: Option<CatalogName>,
    pub namespace: Option<NamespaceName>,
    pub name: ObjectName,
}

impl ObjectRef {
    pub fn new(
        catalog: Option<CatalogName>,
        namespace: Option<NamespaceName>,
        name: ObjectName,
    ) -> Self {
        Self {
            catalog,
            namespace,
            name,
        }
    }

    pub fn bare(name: ObjectName) -> Self {
        Self {
            catalog: None,
            namespace: None,
            name,
        }
    }

    /// Key used by the name index: fold keys joined, absent levels blank.
    pub fn fold_key(&self) -> String {
        format!(
            "{}.{}.{}",
            self.catalog.as_ref().map(|c| c.fold_key()).unwrap_or(""),
            self.namespace.as_ref().map(|n| n.fold_key()).unwrap_or(""),
            self.name.fold_key()
        )
    }

    pub fn display(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.catalog {
            out.push_str(c.spelling());
            out.push('.');
        }
        if let Some(n) = &self.namespace {
            out.push_str(n.spelling());
            out.push('.');
        }
        out.push_str(self.name.spelling());
        out
    }
}

// ---------------------------------------------------------------------------
// Object kinds and capabilities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    Synonym,
    Procedure,
    /// A function whose subtype (scalar/table-valued/aggregate) is unknown —
    /// e.g. parsed from a file without enough context. Metadata-sourced
    /// functions carry one of the specific kinds below.
    Function,
    ScalarFunction,
    TableFunction,
    AggregateFunction,
    Package,
    PackageBody,
    Trigger,
    DomainType,
    CompositeType,
    EnumType,
    TableType,
    Collation,
    Extension,
    Event,
    Constraint,
    Default,
    Rule,
}

/// Single source of "does this dialect have this concept". Validators and
/// collectors query this instead of hard-coding dialect checks.
pub struct DialectCapabilities;

impl DialectCapabilities {
    pub fn supports(dialect: Dialect, kind: ObjectKind) -> bool {
        use Dialect::*;
        use ObjectKind::*;
        match kind {
            Table | View | Index | Trigger => true,
            MaterializedView => matches!(dialect, Postgres | Oracle),
            Sequence => matches!(dialect, SqlServer | Postgres | Oracle),
            Synonym => matches!(dialect, SqlServer | Oracle),
            Procedure => !matches!(dialect, Sqlite),
            Function | ScalarFunction | TableFunction | AggregateFunction => {
                !matches!(dialect, Sqlite)
            }
            Package | PackageBody => matches!(dialect, Oracle),
            DomainType => matches!(dialect, Postgres | SqlServer | Oracle),
            CompositeType => matches!(dialect, Postgres | Oracle),
            EnumType => matches!(dialect, Postgres | MySql),
            TableType => matches!(dialect, SqlServer),
            Collation => matches!(dialect, Postgres | MySql | SqlServer),
            Extension => matches!(dialect, Postgres),
            Event => matches!(dialect, MySql),
            Constraint | Default => true,
            Rule => matches!(dialect, Postgres | SqlServer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_rules_per_dialect() {
        assert_eq!(
            fold_identifier("Person", Dialect::SqlServer, false),
            "person"
        );
        assert_eq!(
            fold_identifier("Person", Dialect::Postgres, false),
            "person"
        );
        assert_eq!(fold_identifier("Person", Dialect::Postgres, true), "Person");
        assert_eq!(fold_identifier("Person", Dialect::Oracle, false), "PERSON");
        assert_eq!(fold_identifier("Person", Dialect::Oracle, true), "Person");
    }

    #[test]
    fn capabilities_sanity() {
        assert!(DialectCapabilities::supports(
            Dialect::Oracle,
            ObjectKind::Package
        ));
        assert!(!DialectCapabilities::supports(
            Dialect::Sqlite,
            ObjectKind::Procedure
        ));
        assert!(DialectCapabilities::supports(
            Dialect::SqlServer,
            ObjectKind::Synonym
        ));
        assert!(!DialectCapabilities::supports(
            Dialect::Postgres,
            ObjectKind::Synonym
        ));
        for d in Dialect::ALL {
            assert!(DialectCapabilities::supports(d, ObjectKind::Table));
        }
    }
}
