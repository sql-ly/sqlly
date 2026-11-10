//! Grounding table and column references against the catalog name index.

use std::collections::{HashMap, HashSet};

use sqlly_lang::sqlmodel::{Dialect, Span, fold_identifier};
use sqlly_lang::sqlparse::{ParsedTree, SymbolUse, UseKind};

use super::index::{CatalogNameIndex, TableResolution};
use super::{
    ReferencedColumn, SqlIssueCode, SqlIssueSeverity, SqlValidationIssue, SqlValidationOptions,
};

/// The grounded table sources of a script, shared by the column, tenant, and
/// destructive checks.
///
/// Every table source records `(statement, resolved entry)` so bare-column and
/// tenant checks can scope per statement; `None` marks a source the validator
/// cannot see through (CTE, cross-database, unresolved).
#[derive(Default)]
pub(super) struct ResolvedSources {
    /// Distinct index entries referenced anywhere in the script.
    pub resolved_objects: Vec<usize>,
    /// Folded alias (or bare table name) → index entry, for qualified columns.
    pub alias_map: HashMap<String, usize>,
    /// One row per table source: `(statement index, entry, span)`.
    pub source_resolutions: Vec<(usize, Option<usize>, Span)>,
}

/// Resolve every table source against the catalog, flagging unknown/ambiguous
/// tables and cross-database references as it goes.
pub(super) fn resolve_table_sources(
    tree: &ParsedTree,
    index: &CatalogNameIndex,
    options: &SqlValidationOptions,
    issues: &mut Vec<SqlValidationIssue>,
) -> ResolvedSources {
    let mut sources = ResolvedSources::default();
    // An empty catalog (never synced for this connection) can't tell a real
    // table from a typo — every lookup would come back "unknown". Skip name
    // resolution entirely rather than flood the editor with false errors.
    if index.is_empty() {
        return sources;
    }

    let dialect = options.dialect;
    let cte_names = collect_cte_names(tree, dialect);
    let default_schema_fold = options
        .default_schema
        .as_deref()
        .map(|s| fold_identifier(s, dialect, false));

    for symbol in &tree.references {
        if symbol.use_kind != UseKind::TableSource {
            continue;
        }
        let reference = &symbol.reference;

        // A single-part name matching a CTE of the same statement is the CTE.
        if reference.namespace.is_none()
            && reference.catalog.is_none()
            && cte_names.contains(&(
                symbol.statement_index,
                reference.name.fold_key().to_string(),
            ))
        {
            sources
                .source_resolutions
                .push((symbol.statement_index, None, symbol.span));
            continue;
        }

        if let CrossDatabase::SkipResolution = check_cross_database(symbol, options, issues) {
            sources
                .source_resolutions
                .push((symbol.statement_index, None, symbol.span));
            continue;
        }

        match index.resolve_table_with_default(reference, default_schema_fold.as_deref()) {
            TableResolution::Resolved(idx) => {
                if !sources.resolved_objects.contains(&idx) {
                    sources.resolved_objects.push(idx);
                }
                if let Some(alias) = &symbol.via_alias {
                    sources
                        .alias_map
                        .insert(fold_identifier(alias, dialect, false), idx);
                }
                // The table's own (folded) name can also qualify columns.
                sources
                    .alias_map
                    .insert(reference.name.fold_key().to_string(), idx);
                sources
                    .source_resolutions
                    .push((symbol.statement_index, Some(idx), symbol.span));
            }
            TableResolution::Unresolved => {
                sources
                    .source_resolutions
                    .push((symbol.statement_index, None, symbol.span));
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::UnresolvedTable,
                    severity: SqlIssueSeverity::Error,
                    message: format!("unknown table or view `{}`", reference.display()),
                    identifier: Some(reference.display()),
                    object_keys: Vec::new(),
                    span: Some(symbol.span),
                });
            }
            TableResolution::Ambiguous(keys) => {
                sources
                    .source_resolutions
                    .push((symbol.statement_index, None, symbol.span));
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::AmbiguousTable,
                    severity: SqlIssueSeverity::Warning,
                    message: format!(
                        "`{}` is ambiguous; matches {} catalog objects",
                        reference.display(),
                        keys.len()
                    ),
                    identifier: Some(reference.display()),
                    object_keys: keys,
                    span: Some(symbol.span),
                });
            }
        }
    }

    sources
}

/// CTE names visible per statement: a FROM that names one is neither a
/// catalog table nor an error.
fn collect_cte_names(tree: &ParsedTree, dialect: Dialect) -> HashSet<(usize, String)> {
    let mut cte_names: HashSet<(usize, String)> = HashSet::new();
    for cte in &tree.cte_definitions {
        cte_names.insert((
            cte.statement_index,
            fold_identifier(&cte.name, dialect, false),
        ));
    }
    cte_names
}

/// What a cross-database qualifier means for the rest of resolution.
enum CrossDatabase {
    /// Same database (or engine where the catalog can still check it):
    /// resolution proceeds normally.
    Proceed,
    /// The reference points outside the connection's database; the catalog
    /// cannot resolve it, so the source is treated as opaque.
    SkipResolution,
}

/// Flag cross-database references, judged by the engine's actual rules.
fn check_cross_database(
    symbol: &SymbolUse,
    options: &SqlValidationOptions,
    issues: &mut Vec<SqlValidationIssue>,
) -> CrossDatabase {
    let dialect = options.dialect;
    let reference = &symbol.reference;
    match dialect {
        Dialect::Postgres => {
            // One PostgreSQL connection cannot query another database at
            // all — a differing catalog qualifier fails at runtime.
            if let (Some(catalog), Some(default_db)) =
                (&reference.catalog, &options.default_database)
                && catalog.fold_key() != fold_identifier(default_db, dialect, false)
            {
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::CrossDatabaseReference,
                    severity: SqlIssueSeverity::Error,
                    message: format!(
                        "PostgreSQL cannot query database `{}` from a connection to `{}`",
                        catalog.spelling(),
                        default_db
                    ),
                    identifier: Some(reference.display()),
                    object_keys: Vec::new(),
                    span: Some(symbol.span),
                });
                return CrossDatabase::SkipResolution;
            }
        }
        Dialect::MySql => {
            // MySQL spells databases as the two-part qualifier
            // (`db.table` parses as a namespace). A differing database is
            // legal SQL the catalog cannot check — warn and stay quiet
            // about resolution instead of flagging a false unknown.
            if let (Some(ns), Some(default_db)) = (&reference.namespace, &options.default_database)
                && ns.fold_key() != fold_identifier(default_db, dialect, false)
            {
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::CrossDatabaseReference,
                    severity: SqlIssueSeverity::Warning,
                    message: format!(
                        "references database `{}` which differs from the connection database `{}`",
                        ns.spelling(),
                        default_db
                    ),
                    identifier: Some(reference.display()),
                    object_keys: Vec::new(),
                    span: Some(symbol.span),
                });
                return CrossDatabase::SkipResolution;
            }
        }
        // SQLite cross-database access rides ATTACH aliases — session
        // state the catalog cannot see — so nothing is flagged.
        Dialect::Sqlite => {}
        _ => {
            if let (Some(catalog), Some(default_db)) =
                (&reference.catalog, &options.default_database)
                && catalog.fold_key() != fold_identifier(default_db, dialect, false)
            {
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::CrossDatabaseReference,
                    severity: SqlIssueSeverity::Warning,
                    message: format!(
                        "references database `{}` which differs from the connection database `{}`",
                        catalog.spelling(),
                        default_db
                    ),
                    identifier: Some(reference.display()),
                    object_keys: Vec::new(),
                    span: Some(symbol.span),
                });
            }
        }
    }
    CrossDatabase::Proceed
}

/// Resolve qualified column references (alias.column / table.column).
pub(super) fn resolve_qualified_columns(
    tree: &ParsedTree,
    index: &CatalogNameIndex,
    alias_map: &HashMap<String, usize>,
    dialect: Dialect,
    issues: &mut Vec<SqlValidationIssue>,
    referenced_columns: &mut Vec<ReferencedColumn>,
) {
    for symbol in &tree.references {
        if symbol.use_kind != UseKind::Column {
            continue;
        }
        let qualifier = match &symbol.via_alias {
            Some(alias) => fold_identifier(alias, dialect, false),
            None => continue,
        };
        // Unknown qualifier (CTE, derived table, subquery alias): skip rather
        // than emit a false positive.
        let Some(&idx) = alias_map.get(&qualifier) else {
            continue;
        };
        let column_fold = symbol.reference.name.fold_key();
        let entry = &index.entries[idx];
        if entry.columns.iter().any(|c| c == column_fold) {
            referenced_columns.push(ReferencedColumn {
                object_key: entry.object_key.clone(),
                column: symbol.reference.name.spelling().to_string(),
            });
        } else {
            issues.push(SqlValidationIssue {
                code: SqlIssueCode::UnresolvedColumn,
                severity: SqlIssueSeverity::Error,
                message: format!(
                    "column `{}` not found on `{}`",
                    symbol.reference.name.spelling(),
                    entry.display_name()
                ),
                identifier: Some(format!(
                    "{}.{}",
                    symbol.via_alias.as_deref().unwrap_or(""),
                    symbol.reference.name.spelling()
                )),
                object_keys: vec![entry.object_key.clone()],
                span: Some(symbol.span),
            });
        }
    }
}

/// Check bare predicate columns (recorded by the parser only in WHERE / JOIN
/// ON, with keywords/functions/subqueries excluded at record time). Resolution
/// is per statement and conservative by construction: any statement whose
/// sources include something the validator cannot see through — a CTE, a
/// derived table, an unresolved or ambiguous table — skips its bare checks
/// entirely rather than guess.
pub(super) fn check_bare_predicate_columns(
    tree: &ParsedTree,
    index: &CatalogNameIndex,
    source_resolutions: &[(usize, Option<usize>, Span)],
    issues: &mut Vec<SqlValidationIssue>,
    referenced_columns: &mut Vec<ReferencedColumn>,
) {
    let (opaque_statements, sources_by_statement) = statement_visibility(tree, source_resolutions);
    for symbol in &tree.references {
        if index.is_empty() || symbol.use_kind != UseKind::ColumnBare {
            continue;
        }
        let statement = symbol.statement_index;
        if opaque_statements.contains(&statement) {
            continue;
        }
        let Some(sources) = sources_by_statement.get(&statement) else {
            continue;
        };
        if sources.is_empty() {
            continue;
        }
        let column_fold = symbol.reference.name.fold_key();
        let hits: Vec<usize> = sources
            .iter()
            .copied()
            .filter(|&idx| index.entries[idx].columns.iter().any(|c| c == column_fold))
            .collect();
        match hits.as_slice() {
            [] => {
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::UnresolvedColumn,
                    severity: SqlIssueSeverity::Warning,
                    message: format!(
                        "column `{}` not found on any table in this statement",
                        symbol.reference.name.spelling()
                    ),
                    identifier: Some(symbol.reference.name.spelling().to_string()),
                    object_keys: sources
                        .iter()
                        .map(|&idx| index.entries[idx].object_key.clone())
                        .collect(),
                    span: Some(symbol.span),
                });
            }
            [only] => {
                referenced_columns.push(ReferencedColumn {
                    object_key: index.entries[*only].object_key.clone(),
                    column: symbol.reference.name.spelling().to_string(),
                });
            }
            many => {
                issues.push(SqlValidationIssue {
                    code: SqlIssueCode::AmbiguousColumn,
                    severity: SqlIssueSeverity::Warning,
                    message: format!(
                        "column `{}` exists on {} tables in this statement; qualify it",
                        symbol.reference.name.spelling(),
                        many.len()
                    ),
                    identifier: Some(symbol.reference.name.spelling().to_string()),
                    object_keys: many
                        .iter()
                        .map(|&idx| index.entries[idx].object_key.clone())
                        .collect(),
                    span: Some(symbol.span),
                });
            }
        }
    }
}

/// Per statement: which resolved sources are visible, and which statements are
/// opaque (contain a source the validator cannot see through, or a derived
/// table alias).
fn statement_visibility(
    tree: &ParsedTree,
    source_resolutions: &[(usize, Option<usize>, Span)],
) -> (HashSet<usize>, HashMap<usize, Vec<usize>>) {
    let mut opaque_statements: HashSet<usize> = HashSet::new();
    let mut sources_by_statement: HashMap<usize, Vec<usize>> = HashMap::new();
    for (statement, entry, _) in source_resolutions {
        match entry {
            Some(idx) => sources_by_statement
                .entry(*statement)
                .or_default()
                .push(*idx),
            None => {
                opaque_statements.insert(*statement);
            }
        }
    }
    for (i, statement) in tree.statements.iter().enumerate() {
        let derived = match statement {
            sqlly_lang::sqlparse::StatementNode::Select(node)
            | sqlly_lang::sqlparse::StatementNode::Delete(node) => !node.derived_aliases.is_empty(),
            _ => false,
        };
        if derived {
            opaque_statements.insert(i);
        }
    }
    (opaque_statements, sources_by_statement)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{SqlIssueCode, SqlIssueSeverity, SqlValidationOptions, validate_sql};
    use crate::catalog::wire::CatalogObject;
    use sqlly_lang::sqlmodel::Dialect;

    #[test]
    fn bare_predicate_column_typo_is_flagged() {
        let store = store_with(&[table("users", &["id", "name"])]);
        let result = validate_sql(
            &store,
            "SELECT name FROM dbo.users WHERE bogus = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            result.issues.iter().any(|i| {
                i.code == SqlIssueCode::UnresolvedColumn
                    && i.severity == SqlIssueSeverity::Warning
                    && i.identifier.as_deref() == Some("bogus")
            }),
            "{result:?}"
        );
    }

    #[test]
    fn bare_predicate_column_that_exists_passes_and_is_referenced() {
        let store = store_with(&[table("users", &["id", "name"])]);
        let result =
            validate_sql(&store, "SELECT name FROM dbo.users WHERE id = 1", &opts()).unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedColumn),
            "{result:?}"
        );
        assert!(
            result.referenced_columns.iter().any(|c| c.column == "id"),
            "{result:?}"
        );
    }

    #[test]
    fn bare_column_on_two_tables_is_ambiguous() {
        let store = store_with(&[
            table("users", &["id", "name"]),
            table("vendors", &["id", "name"]),
        ]);
        let result = validate_sql(
            &store,
            "SELECT 1 FROM dbo.users u JOIN dbo.vendors v ON u.id = v.id WHERE name = 'x'",
            &opts(),
        )
        .unwrap();
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::AmbiguousColumn),
            "{result:?}"
        );
    }

    #[test]
    fn cte_names_are_not_unknown_tables_and_suppress_bare_checks() {
        let store = store_with(&[table("users", &["id", "name"])]);
        let result = validate_sql(
            &store,
            "WITH recent AS (SELECT id FROM dbo.users) SELECT 1 FROM recent WHERE anything_at_all = 1",
            &opts(),
        )
        .unwrap();
        assert_eq!(codes(&result), Vec::<SqlIssueCode>::new(), "{result:?}");
    }

    #[test]
    fn derived_tables_suppress_bare_checks() {
        let store = store_with(&[table("users", &["id", "name"])]);
        let result = validate_sql(
            &store,
            "SELECT 1 FROM dbo.users u, (SELECT 5 AS x) d WHERE mystery = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedColumn),
            "{result:?}"
        );
    }

    fn bare_column_warnings(sql: &str, dialect: Dialect, objects: &[CatalogObject]) -> Vec<String> {
        let store = store_with(objects);
        let options = SqlValidationOptions { dialect, ..opts() };
        validate_sql(&store, sql, &options)
            .unwrap()
            .issues
            .into_iter()
            .filter(|i| i.code == SqlIssueCode::UnresolvedColumn)
            .filter_map(|i| i.identifier)
            .collect()
    }

    /// `CROSS APPLY OPENJSON(…) WITH (…)` and `OUTER APPLY (SELECT …)` add
    /// columns the catalog has never seen; a predicate on them is fine.
    #[test]
    fn apply_sources_suppress_bare_checks() {
        let docs = [table("docs", &["id", "doc"])];
        for sql in [
            "SELECT j.name FROM dbo.docs CROSS APPLY OPENJSON(doc) WITH (name nvarchar(50) '$.name') AS j WHERE name = N'x'",
            "SELECT * FROM dbo.docs CROSS APPLY OPENJSON(doc) WHERE [key] = 'a'",
            "SELECT * FROM dbo.docs d OUTER APPLY (SELECT TOP (1) 5 AS latest) x WHERE latest > 1",
            "SELECT * FROM dbo.docs CROSS APPLY STRING_SPLIT(doc, ',') WHERE value = 'a'",
        ] {
            assert_eq!(
                bare_column_warnings(sql, Dialect::SqlServer, &docs),
                Vec::<String>::new(),
                "{sql}"
            );
        }
        assert_eq!(
            bare_column_warnings(
                "SELECT * FROM docs d CROSS APPLY (SELECT 5 AS latest FROM dual) x WHERE latest > 1",
                Dialect::Oracle,
                &docs
            ),
            Vec::<String>::new()
        );
    }

    /// PIVOT and UNPIVOT replace the source's columns with new ones (the
    /// pivoted values, the unpivot value and name columns).
    #[test]
    fn pivot_and_unpivot_suppress_bare_checks() {
        let part = [table("part", &["partname", "price"])];
        for (dialect, sql) in [
            (
                Dialect::SqlServer,
                "SELECT partname, val FROM dbo.part UNPIVOT (val FOR col IN (price)) AS u WHERE val > 1",
            ),
            (
                Dialect::SqlServer,
                "SELECT [prop] FROM dbo.part PIVOT (AVG(price) FOR partname IN ([prop], [rudder])) AS p WHERE [prop] > 1",
            ),
            (
                Dialect::Oracle,
                "SELECT * FROM part PIVOT (AVG(price) FOR partname IN ('prop' AS prop, 'wing' AS wing)) WHERE prop > 1",
            ),
            (
                Dialect::Oracle,
                "SELECT * FROM part UNPIVOT INCLUDE NULLS (val FOR col IN (price)) WHERE val > 1",
            ),
        ] {
            assert_eq!(
                bare_column_warnings(sql, dialect, &part),
                Vec::<String>::new(),
                "{sql}"
            );
        }
    }

    /// PostgreSQL 16 dropped the alias requirement on subqueries in FROM.
    #[test]
    fn unaliased_derived_table_suppresses_bare_checks() {
        let part = [table("part", &["partname", "price"])];
        assert_eq!(
            bare_column_warnings(
                "SELECT * FROM (SELECT price * 2 AS doubled FROM part) WHERE doubled > 1",
                Dialect::Postgres,
                &part
            ),
            Vec::<String>::new()
        );
    }

    /// MySQL's `FROM dual` is a dummy source with no columns of its own: it
    /// must not count as an opaque source, or every statement that mentions
    /// it stops checking bare columns.
    #[test]
    fn mysql_dual_is_not_an_opaque_source() {
        let part = [table("part", &["partname", "price"])];
        for sql in [
            "SELECT price FROM part WHERE bogus = 1 AND 1 = (SELECT 1 FROM dual)",
            "SELECT price FROM part, dual WHERE bogus = 1",
        ] {
            assert_eq!(
                bare_column_warnings(sql, Dialect::MySql, &part),
                vec!["bogus".to_string()],
                "{sql}"
            );
        }
    }

    /// `dual` contributes nothing wherever it appears as a source: an
    /// UPDATE's or DELETE's subquery must still flag a bare typo.
    #[test]
    fn mysql_dual_in_update_and_delete_sources_keeps_the_check_on() {
        let part = [table("part", &["partname", "price"])];
        for sql in [
            "UPDATE part SET price = (SELECT 1 FROM dual) WHERE bogus = 1",
            "DELETE FROM part WHERE bogus = 1 AND 1 = (SELECT 1 FROM dual)",
        ] {
            assert_eq!(
                bare_column_warnings(sql, Dialect::MySql, &part),
                vec!["bogus".to_string()],
                "{sql}"
            );
        }
    }

    /// The opaque-source rules above must not switch the check off for plain
    /// statements that merely use the same words.
    #[test]
    fn plain_statements_still_flag_bare_typos() {
        let part = [table("part", &["partname", "price", "pivot"])];
        for (dialect, sql) in [
            (
                Dialect::SqlServer,
                "SELECT pivot FROM dbo.part WHERE bogus = 1",
            ),
            (
                Dialect::Postgres,
                "SELECT price FROM part WHERE bogus = 1 AND pivot = 2",
            ),
            (
                Dialect::SqlServer,
                "SELECT * FROM dbo.part p JOIN dbo.part q ON q.price = p.price WHERE bogus = 1",
            ),
        ] {
            assert_eq!(
                bare_column_warnings(sql, dialect, &part),
                vec!["bogus".to_string()],
                "{sql}"
            );
        }
    }

    #[test]
    fn default_schema_disambiguates_unqualified_tables() {
        let mut sales_users = table("users", &["id"]);
        sales_users.schema = Some("sales".into());
        let store = store_with(&[table("users", &["id", "name"]), sales_users]);
        let mut options = opts();
        options.default_schema = Some("dbo".into());
        let result = validate_sql(&store, "SELECT name FROM users", &options).unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::AmbiguousTable),
            "{result:?}"
        );
    }

    #[test]
    fn mysql_cross_database_warns_without_false_unknown_table() {
        let store = store_with(&[table("users", &["id"])]);
        let mut options = opts();
        options.dialect = Dialect::MySql;
        options.default_database = Some("app".into());
        let result = validate_sql(&store, "SELECT 1 FROM other.t2", &options).unwrap();
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::CrossDatabaseReference
                    && i.severity == SqlIssueSeverity::Warning),
            "{result:?}"
        );
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedTable),
            "cross-database MySQL references must not double-report: {result:?}"
        );
    }

    #[test]
    fn postgres_cross_database_is_an_error() {
        let store = store_with(&[table("users", &["id"])]);
        let mut options = opts();
        options.dialect = Dialect::Postgres;
        options.default_database = Some("app".into());
        let result = validate_sql(&store, "SELECT 1 FROM otherdb.public.t2", &options).unwrap();
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::CrossDatabaseReference
                    && i.severity == SqlIssueSeverity::Error),
            "{result:?}"
        );
    }

    #[test]
    fn known_table_resolves_and_is_valid() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "SELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(r.is_valid, "{:?}", r.issues);
        assert!(
            r.referenced_objects
                .iter()
                .any(|o| o.object_key == "dbo.customers")
        );
    }

    #[test]
    fn bare_table_name_resolves() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "SELECT CustomerId FROM Customers", &opts()).unwrap();
        assert!(r.is_valid, "{:?}", r.issues);
    }

    #[test]
    fn unknown_table_reports_error() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "SELECT * FROM dbo.Ghosts", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedTable)
        );
    }

    #[test]
    fn qualified_reference_falls_back_to_bare_name_on_schema_mismatch() {
        // The store's namespace can be absent or spelled differently than the
        // live database; a qualified reference must still resolve by bare name.
        let mut hr_table = table("Department", &["DepartmentId", "Name"]);
        hr_table.schema = None;
        let store = store_with(&[hr_table]);
        let r = validate_sql(
            &store,
            "SELECT DepartmentId FROM HR.Department WHERE DepartmentId = 1",
            &opts(),
        )
        .unwrap();
        assert!(r.is_valid, "{:?}", r.issues);
    }

    #[test]
    fn empty_catalog_skips_table_resolution() {
        // A never-synced catalog can't distinguish real tables from typos;
        // it must not flood the editor with false "unknown table" errors.
        let store = store_with(&[]);
        let r = validate_sql(&store, "SELECT * FROM dbo.Customers", &opts()).unwrap();
        assert!(
            !r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedTable),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn qualified_column_resolves_via_alias() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "SELECT c.Name FROM dbo.Customers AS c WHERE c.CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(r.is_valid, "{:?}", r.issues);
        assert!(r.referenced_columns.iter().any(|c| c.column == "Name"));
    }

    #[test]
    fn unknown_qualified_column_reports_error() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "SELECT c.Bogus FROM dbo.Customers AS c WHERE c.CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedColumn)
        );
    }

    #[test]
    fn unknown_alias_qualifier_is_skipped_not_errored() {
        // `x` is not a known table/alias (e.g. a CTE); don't false-positive.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "SELECT x.foo FROM dbo.Customers AS c WHERE c.CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedColumn)
        );
    }

    #[test]
    fn cross_database_reference_warns() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let options = SqlValidationOptions {
            default_database: Some("appdb".into()),
            ..Default::default()
        };
        let r = validate_sql(
            &store,
            "SELECT CustomerId FROM otherdb.dbo.Customers WHERE CustomerId = 1",
            &options,
        )
        .unwrap();
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::CrossDatabaseReference)
        );
    }

    #[test]
    fn sqlite_attach_alias_is_not_flagged_as_cross_database() {
        let store = store_with(&[table("users", &["id"])]);
        let mut options = opts();
        options.dialect = Dialect::Sqlite;
        options.default_database = Some("main".into());
        let result = validate_sql(&store, "SELECT id FROM other.users", &options).unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| { i.code == SqlIssueCode::CrossDatabaseReference }),
            "{result:?}"
        );
    }
}
