//! Safety checks: tenant scoping and destructive / unconstrained statements.

use std::collections::HashMap;

use sqlly_lang::sqlmodel::{Dialect, Span};
use sqlly_lang::sqlparse::{ParsedTree, StatementNode, TableSource, UseKind};

use crate::catalog::text::{
    has_top_level_keyword, strip_strings_and_comments_for, top_level_equality_on_qualified,
    where_region,
};

use super::index::{CatalogNameIndex, TableResolution};
use super::{SqlIssueCode, SqlIssueSeverity, SqlValidationIssue};

/// Column names that mark a table as tenant-scoped, matching the safety-card
/// convention in `card.rs`. Folded/lowercase forms.
pub(super) const TENANT_COLUMN_FOLDS: [&str; 8] = [
    "tenantid",
    "tenant_id",
    "accountid",
    "account_id",
    "organizationid",
    "organization_id",
    "orgid",
    "org_id",
];

pub(super) fn check_tenant_scope(
    tree: &ParsedTree,
    index: &CatalogNameIndex,
    source_resolutions: &[(usize, Option<usize>, Span)],
    custom_folds: Option<&[String]>,
    fail_closed: bool,
    issues: &mut Vec<SqlValidationIssue>,
) {
    // Predicate column names per statement (bare and qualified alike).
    let mut predicate_columns: HashMap<usize, Vec<String>> = HashMap::new();
    for reference in &tree.references {
        if reference.in_predicate
            && matches!(reference.use_kind, UseKind::Column | UseKind::ColumnBare)
        {
            predicate_columns
                .entry(reference.statement_index)
                .or_default()
                .push(reference.reference.name.fold_key().to_string());
        }
    }
    for (statement, entry, span) in source_resolutions {
        let Some(idx) = entry else { continue };
        // INSERT provides the tenant value in its rows; reads and row
        // mutations are what need the filter.
        if matches!(
            tree.statements.get(*statement),
            Some(StatementNode::Insert(_))
        ) {
            continue;
        }
        let index_entry = &index.entries[*idx];
        let is_tenant_fold = |column: &str| match custom_folds {
            Some(folds) if !folds.is_empty() => folds.iter().any(|fold| fold == column),
            _ => TENANT_COLUMN_FOLDS.contains(&column),
        };
        let Some(tenant_column) = index_entry
            .columns
            .iter()
            .find(|c| is_tenant_fold(c.as_str()))
        else {
            continue;
        };
        // A table keyed by the tenant column IS the tenant registry, not
        // tenant-scoped data.
        if index_entry.primary_key.iter().any(|k| k == tenant_column) {
            continue;
        }
        let satisfied = predicate_columns
            .get(statement)
            .is_some_and(|columns| columns.iter().any(|c| c == tenant_column));
        if !satisfied {
            issues.push(SqlValidationIssue {
                code: SqlIssueCode::TenantScopeMissingPredicate,
                severity: if fail_closed {
                    SqlIssueSeverity::Error
                } else {
                    SqlIssueSeverity::Warning
                },
                message: format!(
                    "`{}` is tenant-scoped by `{tenant_column}`, but the statement has no `{tenant_column}` filter in WHERE or JOIN ON",
                    index_entry.display_name()
                ),
                identifier: Some(index_entry.display_name()),
                object_keys: vec![index_entry.object_key.clone()],
                span: Some(*span),
            });
        }
    }
}

/// Flag *unconstrained* data-changing statements as a caution the user should
/// see before running. An UPDATE or DELETE is unconstrained unless its WHERE
/// clause pins the target table's full primary key to single values (so it can
/// affect at most one row); TRUNCATE/DROP/MERGE are always unconstrained, and
/// ALTER only when it drops a column (an `ADD`/`ALTER COLUMN`/`DROP CONSTRAINT`
/// destroys no data).
/// These are surfaced at [`SqlIssueSeverity::RequireConfirmation`] — an
/// indication, not a hard error, and not a promise that execution will pause.
pub(super) fn check_destructive(
    sql: &str,
    statements: &[StatementNode],
    index: &CatalogNameIndex,
    issues: &mut Vec<SqlValidationIssue>,
    dialect: Dialect,
) {
    for stmt in statements {
        match stmt {
            StatementNode::Update(node) => {
                if !pins_primary_key(sql, node.span, &node.target, index, dialect) {
                    issues.push(SqlValidationIssue {
                        code: SqlIssueCode::UpdateWithoutWhere,
                        severity: SqlIssueSeverity::RequireConfirmation,
                        message: "unconstrained mutation (UPDATE) operation — not limited to a single row by its primary key".into(),
                        identifier: Some(node.target.reference.display()),
                        object_keys: Vec::new(),
                        span: Some(node.target.span),
                    });
                }
            }
            StatementNode::Delete(node) => {
                let target = node.tables.first();
                let pinned = match target {
                    // A session-scoped `#temp` table or `@table` variable
                    // touches no real data, whether or not the object-ref
                    // grammar parsed it.
                    Some(t) if is_session_scoped(&t.reference.display()) => true,
                    Some(t) => pins_primary_key(sql, node.span, t, index, dialect),
                    None => unparseable_delete_is_constrained(sql, node.span, dialect),
                };
                if !pinned {
                    issues.push(SqlValidationIssue {
                        code: SqlIssueCode::DeleteWithoutWhere,
                        severity: SqlIssueSeverity::RequireConfirmation,
                        message: "unconstrained destructive (DELETE) operation — not limited to a single row by its primary key".into(),
                        identifier: target.map(|t| t.reference.display()),
                        object_keys: Vec::new(),
                        span: target.map(|t| t.span),
                    });
                }
            }
            StatementNode::Opaque { head, span } => {
                let verb = head.split_whitespace().next().unwrap_or("");
                // ALTER is only destructive when it drops a column. Adding
                // columns, constraints, or foreign keys, and altering a column's
                // type, all leave existing rows intact — flagging them cried
                // wolf on safe schema changes like `ALTER TABLE … ADD CONSTRAINT
                // … FOREIGN KEY`.
                let flag = match verb {
                    "drop" | "truncate" | "merge" => true,
                    "alter" => alter_drops_column(sql, *span, dialect),
                    _ => false,
                };
                if flag {
                    // MERGE mutates rows; the others are destructive DDL/DML.
                    let family = if verb == "merge" {
                        "mutation"
                    } else {
                        "destructive"
                    };
                    issues.push(SqlValidationIssue {
                        code: SqlIssueCode::DestructiveRequiresConfirmation,
                        severity: SqlIssueSeverity::RequireConfirmation,
                        message: format!(
                            "unconstrained {family} ({}) operation",
                            verb.to_uppercase()
                        ),
                        identifier: None,
                        object_keys: Vec::new(),
                        span: Some(*span),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Whether an `ALTER TABLE` statement drops a column — the only ALTER shape
/// that destroys data. `ADD` (columns, constraints, foreign keys), `ALTER
/// COLUMN`, and `DROP CONSTRAINT` all leave existing rows intact, so they are
/// not flagged. Works off the raw statement text (the parser keeps ALTER as an
/// `Opaque` node whose `head` is only "alter table"), with strings and
/// comments stripped so a column literally named "column" can't fool it.
fn alter_drops_column(sql: &str, span: Span, dialect: Dialect) -> bool {
    let slice = sql.get(span.start..span.end).unwrap_or("");
    let stripped = strip_strings_and_comments_for(slice, dialect).to_lowercase();
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    tokens.windows(2).any(|w| {
        // `COLUMN[Name]` (no space before the bracket) is valid T-SQL, so the
        // keyword may arrive glued to a bracketed identifier. An exact match
        // otherwise, so dropping a *constraint* whose name merely starts with
        // "column" is not flagged.
        w[0] == "drop"
            && (w[1] == "column" || w[1].starts_with("column[") || w[1].starts_with("column\""))
    })
}

/// Whether the statement slice at `span` carries an outer (depth-0) WHERE.
fn has_outer_where(sql: &str, span: Span, dialect: Dialect) -> bool {
    let slice = sql.get(span.start..span.end).unwrap_or("");
    let stripped = strip_strings_and_comments_for(slice, dialect).to_lowercase();
    has_top_level_keyword(&stripped, "where")
}

/// Fallback for a DELETE whose target the object-ref grammar could not parse
/// (most commonly a `#temp` table or `@table` variable). A session-scoped
/// target touches no real data, so it is never flagged; anything else falls
/// back to the unknown-primary-key rule — an outer WHERE is constraint enough.
fn unparseable_delete_is_constrained(sql: &str, span: Span, dialect: Dialect) -> bool {
    let slice = sql.get(span.start..span.end).unwrap_or("");
    let stripped = strip_strings_and_comments_for(slice, dialect).to_lowercase();
    let mut toks = stripped.split_whitespace();
    if toks.next() == Some("delete") {
        let target = match toks.next() {
            Some("from") => toks.next(),
            other => other,
        };
        if target.is_some_and(is_session_scoped) {
            return true;
        }
    }
    has_top_level_keyword(&stripped, "where")
}

/// Whether a table name is a `#temp` table or `@table` variable.
fn is_session_scoped(name: &str) -> bool {
    name.starts_with('#') || name.starts_with('@')
}

/// Whether an UPDATE/DELETE's outer WHERE clause pins the target table's full
/// primary key to single values, so it can match at most one row.
///
/// Graceful degradation: when the target table's primary key is **unknown**
/// (the catalog never reported one, or the reference does not resolve to a
/// single object), any outer WHERE is treated as constraint enough — we cannot
/// prove a single-row match, but flagging every filtered statement would be
/// noise. When the primary key **is** known, the WHERE must carry a top-level
/// equality on every key column and must have no top-level `OR` (which could
/// broaden the match beyond the key).
fn pins_primary_key(
    sql: &str,
    span: Span,
    target: &TableSource,
    index: &CatalogNameIndex,
    dialect: Dialect,
) -> bool {
    // No outer WHERE at all -> unconstrained, regardless of the key.
    if !has_outer_where(sql, span, dialect) {
        return false;
    }
    let slice = sql.get(span.start..span.end).unwrap_or("");
    let stripped = strip_strings_and_comments_for(slice, dialect).to_lowercase();

    let empty: Vec<String> = Vec::new();
    let pk = match index.resolve_table(&target.reference) {
        TableResolution::Resolved(idx) => &index.entries[idx].primary_key,
        _ => &empty,
    };
    if pk.is_empty() {
        // Key unknown: an outer WHERE is accepted as constraint enough.
        return true;
    }

    let region = where_region(&stripped);
    // A top-level OR can broaden the row set beyond the keyed row.
    if has_top_level_keyword(&region, "or") {
        return false;
    }
    // The target's own name and alias are the qualifiers that count: a filter on
    // a different table's same-named column (e.g. `WHERE u.id = 1` in a
    // multi-table UPDATE/DELETE) must NOT read as pinning the target's key.
    let mut quals: Vec<String> = vec![target.reference.name.spelling().to_string()];
    if let Some(alias) = &target.alias {
        quals.push(alias.clone());
    }
    // Every primary-key column must be pinned by a top-level equality on the
    // target (or an unqualified) column.
    pk.iter()
        .all(|col| top_level_equality_on_qualified(&region, col, Some(&quals)))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{SqlIssueCode, SqlIssueSeverity, SqlValidationOptions, validate_sql};
    use super::{alter_drops_column, has_outer_where, unparseable_delete_is_constrained};
    use sqlly_lang::sqlmodel::{Dialect, Span};

    fn full_span(sql: &str) -> Span {
        Span {
            start: 0,
            end: sql.len(),
        }
    }

    // ---- direct unit tests for the pure text helpers ----------------------

    #[test]
    fn alter_drops_column_detects_only_column_drops() {
        for (sql, expected) in [
            ("ALTER TABLE t DROP COLUMN c", true),
            ("ALTER TABLE t DROP COLUMN[c]", true),
            ("ALTER TABLE t DROP CONSTRAINT fk_c", false),
            ("ALTER TABLE t ADD c INT", false),
            // The phrase inside a string literal must not trigger.
            (
                "ALTER TABLE t ADD n NVARCHAR(50) DEFAULT 'drop column'",
                false,
            ),
        ] {
            assert_eq!(
                alter_drops_column(sql, full_span(sql), Dialect::SqlServer),
                expected,
                "{sql}"
            );
        }
    }

    #[test]
    fn has_outer_where_ignores_strings_and_comments() {
        let with_where = "DELETE FROM t WHERE id = 1";
        assert!(has_outer_where(
            with_where,
            full_span(with_where),
            Dialect::SqlServer
        ));
        let in_string = "UPDATE t SET n = 'where am i'";
        assert!(!has_outer_where(
            in_string,
            full_span(in_string),
            Dialect::SqlServer
        ));
        let in_hash_comment = "DELETE FROM t # where id = 1";
        assert!(!has_outer_where(
            in_hash_comment,
            full_span(in_hash_comment),
            Dialect::MySql
        ));
        // A quoted identifier named `where` is a name, not a clause, and a
        // quote character inside a backtick identifier opens no literal.
        let quoted = "DELETE FROM `where`";
        assert!(!has_outer_where(quoted, full_span(quoted), Dialect::MySql));
        let quoted = "DELETE FROM \"where\"";
        assert!(!has_outer_where(
            quoted,
            full_span(quoted),
            Dialect::Postgres
        ));
        let tick = "DELETE FROM t WHERE `'` = 1";
        assert!(has_outer_where(tick, full_span(tick), Dialect::MySql));
        let hidden = "DELETE FROM t -- '\nWHERE id = 1";
        assert!(has_outer_where(hidden, full_span(hidden), Dialect::MySql));
    }

    #[test]
    fn unparseable_delete_session_targets_are_constrained() {
        for sql in ["DELETE FROM #staging", "DELETE FROM @rows", "DELETE #tmp"] {
            assert!(
                unparseable_delete_is_constrained(sql, full_span(sql), Dialect::SqlServer),
                "{sql}"
            );
        }
        let real = "DELETE FROM logs";
        assert!(!unparseable_delete_is_constrained(
            real,
            full_span(real),
            Dialect::SqlServer
        ));
        let filtered = "DELETE FROM logs WHERE id = 1";
        assert!(unparseable_delete_is_constrained(
            filtered,
            full_span(filtered),
            Dialect::SqlServer
        ));
    }

    // ---- tenant-scope checks ----------------------------------------------

    #[test]
    fn tenant_check_is_off_by_default() {
        let store = store_with(&[table_with_pk(
            "orders",
            &["order_id", "tenant_id", "total"],
            &["order_id"],
        )]);
        let result = validate_sql(
            &store,
            "SELECT total FROM dbo.orders",
            &SqlValidationOptions::default(),
        )
        .unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::TenantScopeMissingPredicate),
            "{result:?}"
        );
    }

    #[test]
    fn fail_closed_tenant_miss_is_an_error() {
        let store = store_with(&[table_with_pk(
            "orders",
            &["order_id", "tenant_id", "total"],
            &["order_id"],
        )]);
        let mut options = opts();
        options.require_tenant_filter = true;
        options.fail_closed_tenant_filter = true;
        let result = validate_sql(&store, "SELECT total FROM dbo.orders", &options).unwrap();
        assert!(
            result.issues.iter().any(|i| {
                i.code == SqlIssueCode::TenantScopeMissingPredicate
                    && i.severity == SqlIssueSeverity::Error
            }),
            "{result:?}"
        );
    }

    #[test]
    fn tenant_scoped_table_without_filter_warns() {
        let store = store_with(&[table_with_pk(
            "orders",
            &["order_id", "tenant_id", "total"],
            &["order_id"],
        )]);
        let mut options = opts();
        options.require_tenant_filter = true;
        let result = validate_sql(&store, "SELECT total FROM dbo.orders", &options).unwrap();
        assert!(
            result.issues.iter().any(|i| {
                i.code == SqlIssueCode::TenantScopeMissingPredicate
                    && i.severity == SqlIssueSeverity::Warning
                    && i.message.contains("tenant_id")
            }),
            "{result:?}"
        );
    }

    #[test]
    fn tenant_filter_in_where_or_join_on_satisfies_the_guard() {
        let store = store_with(&[
            table_with_pk("orders", &["order_id", "tenant_id", "total"], &["order_id"]),
            table("users", &["id", "tenant"]),
        ]);
        for sql in [
            "SELECT total FROM dbo.orders WHERE tenant_id = 7",
            "SELECT total FROM dbo.orders o JOIN dbo.users u ON o.tenant_id = u.id",
        ] {
            let result = validate_sql(&store, sql, &opts()).unwrap();
            assert!(
                !result
                    .issues
                    .iter()
                    .any(|i| i.code == SqlIssueCode::TenantScopeMissingPredicate),
                "{sql}: {result:?}"
            );
        }
    }

    #[test]
    fn tenant_registry_table_and_disabled_option_stay_quiet() {
        let store = store_with(&[table_with_pk(
            "tenants",
            &["tenant_id", "name"],
            &["tenant_id"],
        )]);
        let result = validate_sql(&store, "SELECT name FROM dbo.tenants", &opts()).unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::TenantScopeMissingPredicate),
            "the tenants registry itself is not tenant-scoped data: {result:?}"
        );

        let scoped = store_with(&[table_with_pk(
            "orders",
            &["order_id", "tenant_id"],
            &["order_id"],
        )]);
        let mut options = opts();
        options.require_tenant_filter = false;
        let result = validate_sql(&scoped, "SELECT 1 FROM dbo.orders", &options).unwrap();
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::TenantScopeMissingPredicate),
            "{result:?}"
        );
    }

    // ---- destructive / unconstrained checks -------------------------------

    #[test]
    fn mysql_hash_comment_does_not_mask_an_unconstrained_delete() {
        let store = store_with(&[table("logs", &["id"])]);
        let mut options = opts();
        options.dialect = Dialect::MySql;
        let result = validate_sql(&store, "DELETE FROM logs # where id = 1", &options).unwrap();
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DeleteWithoutWhere),
            "a hash-commented WHERE is not a WHERE on MySQL: {result:?}"
        );
    }

    #[test]
    fn update_without_where_is_error() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(&store, "UPDATE dbo.Customers SET Name = 'x'", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UpdateWithoutWhere)
        );
    }

    #[test]
    fn update_with_where_has_no_where_error() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Customers SET Name = 'x' WHERE CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UpdateWithoutWhere)
        );
    }

    #[test]
    fn delete_without_where_is_error() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "DELETE FROM dbo.Customers", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DeleteWithoutWhere)
        );
    }

    #[test]
    fn where_inside_string_does_not_count() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Customers SET Name = 'where am i'",
            &opts(),
        )
        .unwrap();
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UpdateWithoutWhere)
        );
    }

    #[test]
    fn drop_requires_confirmation() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "DROP TABLE dbo.Customers", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(r.requires_confirmation);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DestructiveRequiresConfirmation)
        );
    }

    #[test]
    fn update_with_only_subquery_where_is_still_error() {
        // The outer UPDATE has no WHERE; the WHERE lives inside a scalar
        // subquery. This must still flag UpdateWithoutWhere (regression for the
        // subquery-bypass false negative).
        let store = store_with(&[
            table("Orders", &["OrderId", "Name", "CustomerId"]),
            table("Customers", &["CustomerId", "Name"]),
        ]);
        let sql =
            "UPDATE dbo.Orders SET Name = (SELECT Name FROM dbo.Customers WHERE CustomerId = 1)";
        let r = validate_sql(&store, sql, &opts()).unwrap();
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn delete_with_outer_where_and_subquery_is_safe() {
        let store = store_with(&[
            table("Orders", &["OrderId", "CustomerId"]),
            table("Customers", &["CustomerId", "Name"]),
        ]);
        let sql = "DELETE FROM dbo.Orders WHERE CustomerId IN (SELECT CustomerId FROM dbo.Customers WHERE Name = 'x')";
        let r = validate_sql(&store, sql, &opts()).unwrap();
        assert!(
            !r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn merge_requires_confirmation() {
        let store = store_with(&[
            table("Orders", &["OrderId", "CustomerId"]),
            table("Customers", &["CustomerId"]),
        ]);
        let sql = "MERGE INTO dbo.Orders AS t USING dbo.Customers AS s ON t.CustomerId = s.CustomerId WHEN MATCHED THEN UPDATE SET t.CustomerId = s.CustomerId";
        let r = validate_sql(&store, sql, &opts()).unwrap();
        assert!(r.requires_confirmation, "{:?}", r.issues);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn update_pinned_to_primary_key_is_constrained() {
        let store = store_with(&[table_with_pk(
            "Customers",
            &["CustomerId", "Name"],
            &["CustomerId"],
        )]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Customers SET Name = 'x' WHERE CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn update_on_non_key_column_is_unconstrained() {
        let store = store_with(&[table_with_pk(
            "Customers",
            &["CustomerId", "Name"],
            &["CustomerId"],
        )]);
        // A WHERE on a non-key column can match many rows -> unconstrained.
        let r = validate_sql(
            &store,
            "UPDATE dbo.Customers SET Name = 'x' WHERE Name = 'old'",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("unconstrained mutation (UPDATE)")),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn delete_pinned_to_primary_key_is_constrained() {
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE CustomerId = 42",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn delete_on_non_key_column_is_unconstrained() {
        let store = store_with(&[table_with_pk(
            "Customers",
            &["CustomerId", "Region"],
            &["CustomerId"],
        )]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE Region = 'west'",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message.contains("unconstrained destructive (DELETE)")),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn composite_primary_key_requires_every_column() {
        let store = store_with(&[table_with_pk(
            "OrderLines",
            &["OrderId", "LineNo", "Qty"],
            &["OrderId", "LineNo"],
        )]);
        // Only one of the two key columns is pinned -> can match many lines.
        let partial = validate_sql(
            &store,
            "UPDATE dbo.OrderLines SET Qty = 1 WHERE OrderId = 5",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&partial, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            partial.issues
        );
        // Both key columns pinned -> at most one row.
        let full = validate_sql(
            &store,
            "UPDATE dbo.OrderLines SET Qty = 1 WHERE OrderId = 5 AND LineNo = 2",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&full, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            full.issues
        );
    }

    #[test]
    fn top_level_or_broadens_beyond_the_key() {
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE CustomerId = 1 OR CustomerId = 2",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn unknown_primary_key_falls_back_to_has_where() {
        // The table's key was never reported: any outer WHERE is accepted so we
        // do not flag every filtered statement as unconstrained.
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let constrained = validate_sql(
            &store,
            "UPDATE dbo.Customers SET Name = 'x' WHERE Name = 'old'",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&constrained, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            constrained.issues
        );
        // No WHERE at all is still unconstrained, key or no key.
        let bare = validate_sql(&store, "UPDATE dbo.Customers SET Name = 'x'", &opts()).unwrap();
        assert!(
            has_code(&bare, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            bare.issues
        );
    }

    #[test]
    fn truncate_is_unconstrained_destructive() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "TRUNCATE TABLE dbo.Customers", &opts()).unwrap();
        assert!(has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation));
        assert!(
            r.issues
                .iter()
                .any(|i| i.message == "unconstrained destructive (TRUNCATE) operation"),
            "{:?}",
            r.issues
        );
        // The notice no longer promises confirmation in its text.
        assert!(
            !r.issues.iter().any(|i| i.message.contains("confirmation")),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alter_add_constraint_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        // Adding a foreign key destroys no data — it must not be flagged as an
        // unconstrained destructive change.
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] ADD CONSTRAINT [FK_Job_Tenant] FOREIGN KEY ([TenantId]) REFERENCES [Shared].[Tenant] ([TenantId])",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alter_drop_column_is_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] DROP COLUMN [TenantId]",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.message == "unconstrained destructive (ALTER) operation"),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alter_drop_constraint_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        // Dropping a constraint is not dropping a column — no data loss.
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] DROP CONSTRAINT [FK_Job_Tenant]",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alter_add_column_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] ADD [Notes] NVARCHAR(200) NULL",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alter_alter_column_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        // Widening/retyping a column keeps its rows — no confirmation.
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] ALTER COLUMN [Title] NVARCHAR(400) NOT NULL",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn drop_column_text_inside_string_or_comment_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        for sql in [
            // The phrase lives in a default literal, not the statement.
            "ALTER TABLE [HR].[Job] ADD [Note] NVARCHAR(50) DEFAULT 'drop column'",
            // ... or in comments.
            "ALTER TABLE [HR].[Job] ADD [Note] INT -- drop column later",
            "ALTER TABLE [HR].[Job] /* drop column note */ ADD [Note] INT",
            // A multi-line block comment inside the ALTER span.
            "ALTER TABLE [HR].[Job]\n/* todo:\n   drop column TenantId\n   next release */\nADD [Note] INT",
            // T-SQL block comments nest: the phrase after the inner `*/` is
            // still comment text, not statement text.
            "ALTER TABLE [HR].[Job] /* note /* inner */ drop column later */ ADD [Note] INT",
        ] {
            let r = validate_sql(&store, sql, &opts()).unwrap();
            assert!(
                !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
                "{sql}: {:?}",
                r.issues
            );
        }
    }

    #[test]
    fn drop_column_after_comments_is_still_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        // Comment stripping must not swallow the real operation that follows.
        for sql in [
            "ALTER TABLE [HR].[Job] /* keep */ DROP COLUMN [TenantId]",
            "ALTER TABLE [HR].[Job] /* a /* nested */ b */ DROP COLUMN [TenantId]",
            "ALTER TABLE [HR].[Job]\n-- see ticket 42\nDROP COLUMN [TenantId]",
        ] {
            let r = validate_sql(&store, sql, &opts()).unwrap();
            assert!(
                has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
                "{sql}: {:?}",
                r.issues
            );
        }
    }

    #[test]
    fn drop_column_with_glued_bracket_or_comment_is_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        for sql in [
            // `COLUMN[Name]` — no space before the bracketed identifier.
            "ALTER TABLE [HR].[Job] DROP COLUMN[TenantId]",
            // A block comment between the keywords must not hide the drop.
            "ALTER TABLE [HR].[Job] DROP/*x*/COLUMN [TenantId]",
        ] {
            let r = validate_sql(&store, sql, &opts()).unwrap();
            assert!(
                has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
                "{sql}: {:?}",
                r.issues
            );
        }
    }

    #[test]
    fn drop_constraint_named_like_column_is_not_destructive() {
        let store = store_with(&[table("Job", &["JobId"])]);
        // `ALTER TABLE … DROP <name>` drops a constraint; one whose name starts
        // with "column" must not read as a column drop.
        let r = validate_sql(
            &store,
            "ALTER TABLE [HR].[Job] DROP column_fk_legacy",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DestructiveRequiresConfirmation),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn parenthesized_primary_key_equality_pins() {
        // `WHERE (id = 1)` — the outer balanced parens must not hide the pin.
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE (CustomerId = 1)",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn bracketed_reserved_word_primary_key_pins() {
        // `[Order]` is a bracket-quoted reserved word used as the key column.
        let store = store_with(&[table_with_pk("Lines", &["Order", "Qty"], &["Order"])]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Lines SET Qty = 1 WHERE [Order] = 7",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn other_table_qualified_key_does_not_pin_target() {
        // A multi-table UPDATE where the only `Id = 1` filter is on the *joined*
        // table `c`, not the target `Orders`, so the target is not pinned.
        let store = store_with(&[
            table_with_pk("Orders", &["Id", "CustomerId"], &["Id"]),
            table_with_pk("Customers", &["Id"], &["Id"]),
        ]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Orders SET CustomerId = 0 FROM dbo.Orders o JOIN dbo.Customers c ON o.CustomerId = c.Id WHERE c.Id = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn alias_qualified_target_key_pins() {
        let store = store_with(&[table_with_pk(
            "Customers",
            &["CustomerId", "Name"],
            &["CustomerId"],
        )]);
        let r = validate_sql(
            &store,
            "UPDATE c SET c.Name = 'x' FROM dbo.Customers c WHERE c.CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn bracketed_multipart_target_key_pins() {
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE [Customers].[CustomerId] = 1",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn key_equals_itself_does_not_pin() {
        // `id = id` is a tautology matching every row — never a single-row pin.
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE CustomerId = CustomerId",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn key_equals_other_column_does_not_pin() {
        // Equating the key to another column can match many rows.
        let store = store_with(&[table_with_pk(
            "Customers",
            &["CustomerId", "MirrorId"],
            &["CustomerId"],
        )]);
        let r = validate_sql(
            &store,
            "UPDATE dbo.Customers SET MirrorId = 0 WHERE CustomerId = MirrorId",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn key_in_list_does_not_pin() {
        // `IN (...)` has no top-level `=`, so it is not treated as a single-row pin.
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE CustomerId IN (1, 2, 3)",
            &opts(),
        )
        .unwrap();
        assert!(
            has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn key_equals_parameter_pins() {
        // A parameterized single-value equality is a legitimate single-row pin.
        let store = store_with(&[table_with_pk("Customers", &["CustomerId"], &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "DELETE FROM dbo.Customers WHERE CustomerId = @id",
            &opts(),
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn procedure_body_dml_is_not_flagged_unconstrained() {
        // Deploying a procedure executes no DML: the UPDATE/DELETE inside its
        // body must not trigger unconstrained-mutation confirmations, and the
        // body statements must not count as extra top-level statements.
        let store = store_with(&[
            table_with_pk("Customers", &["CustomerId", "Name"], &["CustomerId"]),
            table("AuditLog", &["LogId", "LoggedAt"]),
        ]);
        let sql = "CREATE OR ALTER PROCEDURE dbo.Cleanup AS\nBEGIN\n  UPDATE dbo.Customers SET Name = 'x';\n  DELETE FROM dbo.AuditLog;\nEND";
        let r = validate_sql(&store, sql, &opts()).unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
        assert!(
            !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
            "{:?}",
            r.issues
        );
        assert!(
            !has_code(&r, SqlIssueCode::MultipleStatements),
            "{:?}",
            r.issues
        );
        assert!(!r.requires_confirmation, "{:?}", r.issues);
    }

    #[test]
    fn dml_after_procedure_batch_is_still_flagged() {
        // Only the routine's own body is exempt: real DML in the next GO batch
        // still gets the unconstrained check.
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let sql = "CREATE PROCEDURE dbo.P AS\nBEGIN\n  SELECT 1;\nEND\nGO\nUPDATE dbo.Customers SET Name = 'x'";
        let options = SqlValidationOptions {
            allow_multiple_statements: true,
            ..opts()
        };
        let r = validate_sql(&store, sql, &options).unwrap();
        assert!(
            has_code(&r, SqlIssueCode::UpdateWithoutWhere),
            "{:?}",
            r.issues
        );
    }

    #[test]
    fn temp_table_delete_is_never_flagged() {
        // `#temp` tables and `@table` variables are session-scoped; clearing
        // them touches no real data.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        for sql in [
            "DELETE FROM #staging",
            "DELETE FROM #staging WHERE id = 1",
            "DELETE FROM @rows",
            "DELETE #staging",
        ] {
            let r = validate_sql(&store, sql, &opts()).unwrap();
            assert!(
                !has_code(&r, SqlIssueCode::DeleteWithoutWhere),
                "{sql}: {:?}",
                r.issues
            );
        }
    }
}
