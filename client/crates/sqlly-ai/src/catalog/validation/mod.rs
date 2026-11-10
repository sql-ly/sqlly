//! Deterministic, catalog-grounded SQL validation.
//!
//! [`validate_sql`] parses a statement with [`sqlly_lang::sqlparse`], resolves every
//! table and qualified-column reference against the live catalog, and runs a set
//! of deterministic safety checks (tenant scoping, destructive statements,
//! cross-database references). It never calls an LLM and never executes SQL — it
//! only inspects text against known schema metadata.
//!
//! The result distinguishes two notions that callers must not conflate:
//! - `is_valid`: the statement parses and every reference resolves, with no
//!   blocking errors and nothing awaiting confirmation.
//! - `requires_confirmation`: the statement is structurally fine but does
//!   something destructive (DROP/ALTER/TRUNCATE) that a human should confirm.
//!
//! Split by concern: `index` builds the fold-aware catalog name index,
//! `resolve` grounds table/column references against it, and `safety` holds
//! the tenant-scope and destructive-statement checks. This module owns the
//! issue/result types and the [`validate_sql`] orchestration.

mod index;
#[cfg(test)]
mod newer_syntax;
mod resolve;
mod safety;

use sqlly_lang::sqlmodel::{Dialect, Span};
use sqlly_lang::sqlparse::parse;

use crate::catalog::intent::{SqlIntent, intent_from_statements};
use crate::catalog::prompt::GroundedObject;
use crate::catalog::store::{CatalogResult, CatalogStore};

use index::CatalogNameIndex;

/// Severity of a validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlIssueSeverity {
    /// Informational; does not affect validity.
    Info,
    /// Non-blocking risk worth surfacing.
    Warning,
    /// Blocking: the statement is invalid or unsafe to run as written.
    Error,
    /// Structurally fine, but a human must confirm before running.
    RequireConfirmation,
}

/// Stable issue code. UI copy, telemetry, and future persisted safety rules can
/// key off these without parsing message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlIssueCode {
    ParseError,
    EmptyStatement,
    MultipleStatements,
    UnresolvedTable,
    AmbiguousTable,
    UnresolvedColumn,
    AmbiguousColumn,
    TenantScopeMissingPredicate,
    UpdateWithoutWhere,
    DeleteWithoutWhere,
    DestructiveRequiresConfirmation,
    CrossDatabaseReference,
    /// The text reads differently under a server setting the client cannot
    /// see (sql_mode, standard_conforming_strings, an executable comment's
    /// version gate, …) or under the connection's engine versus the parser's
    /// grammar family, and the other reading holds statement keywords the
    /// parsed reading does not — so the checks above may not have seen every
    /// statement that would run.
    LexicalAmbiguity,
}

impl SqlIssueCode {
    /// Whether an `Error`-severity issue with this code is worth feeding back to
    /// the model for an automatic correction attempt.
    ///
    /// Only *grounding/shape* errors are retryable — problems the model can fix
    /// by rewriting against real catalog identifiers without changing what the
    /// user asked for. Destructive-intent safety errors
    /// ([`SqlIssueCode::UpdateWithoutWhere`] / [`SqlIssueCode::DeleteWithoutWhere`])
    /// are deliberately **not** retryable: a "fix" would mean inventing a
    /// predicate the user never requested, silently changing the statement's
    /// meaning. Those stay as findings for a human to resolve. Confirmation-gated
    /// destructive operations are handled separately via `requires_confirmation`.
    pub fn is_retryable(self) -> bool {
        match self {
            SqlIssueCode::EmptyStatement
            | SqlIssueCode::MultipleStatements
            | SqlIssueCode::UnresolvedTable
            | SqlIssueCode::UnresolvedColumn => true,
            SqlIssueCode::ParseError
            | SqlIssueCode::AmbiguousTable
            | SqlIssueCode::AmbiguousColumn
            | SqlIssueCode::TenantScopeMissingPredicate
            | SqlIssueCode::UpdateWithoutWhere
            | SqlIssueCode::DeleteWithoutWhere
            | SqlIssueCode::DestructiveRequiresConfirmation
            | SqlIssueCode::CrossDatabaseReference
            | SqlIssueCode::LexicalAmbiguity => false,
        }
    }
}

/// A single finding from validation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SqlValidationIssue {
    pub code: SqlIssueCode,
    pub severity: SqlIssueSeverity,
    pub message: String,
    /// The identifier the issue is about (e.g. `dbo.Customers`, `c.TenantId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Catalog object keys implicated by this issue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_keys: Vec<String>,
    /// Byte-offset span of the offending token in the source SQL, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

/// A column reference that resolved against a catalog object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferencedColumn {
    pub object_key: String,
    pub column: String,
}

/// Options that shape validation behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlValidationOptions {
    pub dialect: Dialect,
    /// The connection's engine, when known. The parser reads text with the
    /// editor grammar for `dialect`; with the engine known, validation also
    /// checks that every reading the engine could apply (`sqlly_lang::seclex`)
    /// agrees with the parsed one, and flags the text when it does not.
    pub engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
    /// Default database/catalog; used to flag cross-database references.
    pub default_database: Option<String>,
    /// Default schema/namespace: disambiguates unqualified table names that
    /// match objects in more than one schema.
    pub default_schema: Option<String>,
    /// When false, more than one statement is reported as an error.
    pub allow_multiple_statements: bool,
    /// When true, tenant-scoped tables without a tenant predicate are flagged.
    /// Off by default: tenant scoping only surfaces when a caller explicitly
    /// enables it for the database.
    pub require_tenant_filter: bool,
    /// When true with `require_tenant_filter`, a missing tenant predicate is an
    /// error instead of a warning (Required tenant-scoping mode).
    pub fail_closed_tenant_filter: bool,
    /// Custom tenant-discriminator column names (already folded/lowercase).
    /// `None` uses the built-in [`safety::TENANT_COLUMN_FOLDS`] convention
    /// list; a per-database configuration narrows detection to exactly its
    /// column.
    pub tenant_column_folds: Option<Vec<String>>,
    /// When true, destructive / unconstrained statements (UPDATE/DELETE not
    /// pinned to a single row by primary key, TRUNCATE/DROP/MERGE, and
    /// column-dropping ALTER) are flagged at
    /// [`SqlIssueSeverity::RequireConfirmation`]. Off by default: hand-written
    /// SQL is not warned on unless the caller opts in (the app wires this to
    /// the connection's "confirm data changes" safety flag); the AI generation
    /// path always turns it on so model-written mutations stay human-gated.
    pub flag_unconstrained_mutations: bool,
}

impl Default for SqlValidationOptions {
    fn default() -> Self {
        Self {
            dialect: Dialect::SqlServer,
            engine: None,
            default_database: None,
            default_schema: None,
            allow_multiple_statements: false,
            require_tenant_filter: false,
            fail_closed_tenant_filter: false,
            tenant_column_folds: None,
            flag_unconstrained_mutations: false,
        }
    }
}

/// The outcome of validating a statement.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SqlValidationResult {
    /// Parses, resolves, and has no blocking errors or pending confirmations.
    pub is_valid: bool,
    /// Carries a destructive operation that a human must confirm.
    pub requires_confirmation: bool,
    pub detected_intent: SqlIntent,
    pub issues: Vec<SqlValidationIssue>,
    pub referenced_objects: Vec<GroundedObject>,
    pub referenced_columns: Vec<ReferencedColumn>,
}

impl SqlValidationResult {
    /// Whether this result's blocking errors are **all** safe to feed back to
    /// the model for an automatic correction attempt.
    ///
    /// True only when the statement is **not** awaiting human confirmation,
    /// carries at least one `Error`, and **every** `Error` has a
    /// [`SqlIssueCode::is_retryable`] code. If even one blocking error is
    /// non-retryable (e.g. a destructive-intent `DELETE`/`UPDATE` without
    /// `WHERE`), this returns false so the retry loop never hands the model a
    /// correction prompt that could invent a predicate the user never asked for
    /// — even when a retryable error (an unresolved table/column) is present in
    /// the same statement. Confirmation-gated destructive SQL and warning-only
    /// results also return false.
    pub fn has_only_retryable_errors(&self) -> bool {
        if self.requires_confirmation {
            return false;
        }
        let mut saw_error = false;
        for issue in &self.issues {
            if issue.severity == SqlIssueSeverity::Error {
                saw_error = true;
                if !issue.code.is_retryable() {
                    return false;
                }
            }
        }
        saw_error
    }

    fn single_issue(
        code: SqlIssueCode,
        severity: SqlIssueSeverity,
        message: impl Into<String>,
    ) -> Self {
        let requires_confirmation = severity == SqlIssueSeverity::RequireConfirmation;
        let is_valid = severity != SqlIssueSeverity::Error && !requires_confirmation;
        Self {
            is_valid,
            requires_confirmation,
            detected_intent: SqlIntent::Unknown,
            issues: vec![SqlValidationIssue {
                code,
                severity,
                message: message.into(),
                identifier: None,
                object_keys: Vec::new(),
                span: None,
            }],
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        }
    }
}

/// Validate `sql` against the catalog. Never executes SQL; never calls an LLM.
pub fn validate_sql(
    store: &CatalogStore,
    sql: &str,
    options: &SqlValidationOptions,
) -> CatalogResult<SqlValidationResult> {
    if sql.trim().is_empty() {
        return Ok(SqlValidationResult::single_issue(
            SqlIssueCode::EmptyStatement,
            SqlIssueSeverity::Error,
            "no SQL statement to validate",
        ));
    }

    let dialect = options.dialect;
    let tree = parse(sql, dialect);

    if tree.statements.is_empty() {
        return Ok(SqlValidationResult::single_issue(
            SqlIssueCode::EmptyStatement,
            SqlIssueSeverity::Error,
            "no recognizable SQL statement",
        ));
    }

    let intent = intent_from_statements(&tree.statements);
    let mut issues: Vec<SqlValidationIssue> = Vec::new();

    note_parse_recovery(&tree, &mut issues);
    check_statement_count(&tree, options, &mut issues);
    check_lexical_agreement(sql, options, &mut issues);

    let index = CatalogNameIndex::load(store, dialect)?;
    let sources = resolve::resolve_table_sources(&tree, &index, options, &mut issues);

    let mut referenced_columns: Vec<ReferencedColumn> = Vec::new();
    resolve::resolve_qualified_columns(
        &tree,
        &index,
        &sources.alias_map,
        dialect,
        &mut issues,
        &mut referenced_columns,
    );
    resolve::check_bare_predicate_columns(
        &tree,
        &index,
        &sources.source_resolutions,
        &mut issues,
        &mut referenced_columns,
    );

    // Tenant scope: a resolved source with a tenant-discriminator column must
    // see that column in one of the statement's predicates (WHERE / JOIN ON /
    // HAVING). Tables KEYED by the tenant column (the tenants/accounts table
    // itself) are exempt — only tenant-scoped data rows need the filter.
    if options.require_tenant_filter && !index.is_empty() {
        safety::check_tenant_scope(
            &tree,
            &index,
            &sources.source_resolutions,
            options.tenant_column_folds.as_deref(),
            options.fail_closed_tenant_filter,
            &mut issues,
        );
    }

    // Safety: destructive / unconstrained statements (opt-in; see the option's
    // doc for who turns it on).
    if options.flag_unconstrained_mutations {
        safety::check_destructive(sql, &tree.statements, &index, &mut issues, dialect);
    }

    let referenced_objects = sources
        .resolved_objects
        .iter()
        .map(|&idx| index.entries[idx].grounded())
        .collect();

    let requires_confirmation = issues
        .iter()
        .any(|i| i.severity == SqlIssueSeverity::RequireConfirmation);
    let is_valid =
        !requires_confirmation && !issues.iter().any(|i| i.severity == SqlIssueSeverity::Error);

    Ok(SqlValidationResult {
        is_valid,
        requires_confirmation,
        detected_intent: intent,
        issues,
        referenced_objects,
        referenced_columns,
    })
}

/// Fail closed when the parsed reading may not be the one the server uses.
///
/// Everything above judges the parser's reading of the text — the editor
/// grammar for `options.dialect`, under default server settings. When the
/// connection's engine is known, the security lexer also reads the text
/// every way that engine could (unknown `sql_mode`, executable-comment
/// versions, the engine's own quoting where the parser borrows a family's
/// grammar). If some reading holds a statement keyword the parsed reading
/// does not — a statement the parser never saw — the tenant-scope and
/// destructive checks cannot vouch for the text: under fail-closed tenant
/// scoping that blocks the run, and otherwise it is surfaced for review.
fn check_lexical_agreement(
    sql: &str,
    options: &SqlValidationOptions,
    issues: &mut Vec<SqlValidationIssue>,
) {
    use sqlly_lang::seclex;
    use sqlly_lang::tokenizer::TokenKind;
    use std::collections::HashMap;
    let Some(db) = options.engine else { return };
    if db.is_command_engine() {
        return;
    }
    let keyword_counts = |view: &seclex::View| {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for token in view.tokens() {
            if token.kind == TokenKind::Keyword {
                *counts.entry(token.text.to_ascii_uppercase()).or_default() += 1;
            }
        }
        counts
    };
    let parsed = keyword_counts(&seclex::editor_view(sql, options.dialect));
    let views = seclex::views_for(sql, db);
    let finding = views.ambiguity().or_else(|| {
        views.views.iter().find_map(|view| {
            keyword_counts(view)
                .into_iter()
                .find(|(word, n)| parsed.get(word).copied().unwrap_or(0) < *n)
                .map(|(word, _)| {
                    format!(
                        "{} reads {word} where the parsed reading does not",
                        view.reading
                    )
                })
        })
    });
    let Some(finding) = finding else { return };
    let severity = if options.require_tenant_filter && options.fail_closed_tenant_filter {
        SqlIssueSeverity::Error
    } else if options.flag_unconstrained_mutations {
        SqlIssueSeverity::RequireConfirmation
    } else {
        SqlIssueSeverity::Warning
    };
    issues.push(SqlValidationIssue {
        code: SqlIssueCode::LexicalAmbiguity,
        severity,
        // "tenant-scoped" is the phrase the app's fail-closed run gate keys
        // on: a statement the checks did not see may skip the tenant filter.
        message: format!(
            "this SQL can be read more than one way ({finding}); tenant-scoped and destructive-statement checks judged only the default reading, so a statement they did not see could run"
        ),
        identifier: None,
        object_keys: Vec::new(),
        span: None,
    });
}

/// Recovery actions mean the parser had to guess — surface as a warning, not
/// a hard error, since it still produced a usable tree.
fn note_parse_recovery(
    tree: &sqlly_lang::sqlparse::ParsedTree,
    issues: &mut Vec<SqlValidationIssue>,
) {
    if !tree.recovery.is_empty() {
        issues.push(SqlValidationIssue {
            code: SqlIssueCode::ParseError,
            severity: SqlIssueSeverity::Warning,
            message: format!(
                "SQL parsed with {} recovery action(s); syntax may be malformed",
                tree.recovery.len()
            ),
            identifier: None,
            object_keys: Vec::new(),
            span: None,
        });
    }
}

/// When multiple statements are allowed (the editor validates whole script
/// buffers), their presence is normal and not worth an issue at all.
fn check_statement_count(
    tree: &sqlly_lang::sqlparse::ParsedTree,
    options: &SqlValidationOptions,
    issues: &mut Vec<SqlValidationIssue>,
) {
    if tree.statements.len() > 1 && !options.allow_multiple_statements {
        issues.push(SqlValidationIssue {
            code: SqlIssueCode::MultipleStatements,
            severity: SqlIssueSeverity::Error,
            message: format!(
                "expected a single statement, found {}",
                tree.statements.len()
            ),
            identifier: None,
            object_keys: Vec::new(),
            span: None,
        });
    }
}

/// Shared fixtures for the validation test modules.
#[cfg(test)]
pub(super) mod test_support {
    use super::{SqlIssueCode, SqlValidationOptions, SqlValidationResult};
    use crate::catalog::store::CatalogStore;
    use crate::catalog::wire::{
        CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
    };

    pub fn col(name: &str, ordinal: i64) -> CatalogColumn {
        CatalogColumn {
            name: name.into(),
            type_name: Some("INT".into()),
            ordinal,
            nullable: false,
        }
    }

    pub fn table(name: &str, cols: &[&str]) -> CatalogObject {
        table_with_pk(name, cols, &[])
    }

    pub fn table_with_pk(name: &str, cols: &[&str], pk: &[&str]) -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind: CatalogObjectKind::Table,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| col(c, i as i64 + 1))
                .collect(),
            primary_key: pk.iter().map(|c| c.to_string()).collect(),
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        }
    }

    pub fn store_with(objects: &[CatalogObject]) -> CatalogStore {
        let store = CatalogStore::open_in_memory().unwrap();
        store.sync(objects).unwrap();
        store
    }

    pub fn opts() -> SqlValidationOptions {
        SqlValidationOptions {
            // Most safety tests exercise the destructive/unconstrained check;
            // it is opt-in in production (off by default for hand-written SQL).
            flag_unconstrained_mutations: true,
            ..SqlValidationOptions::default()
        }
    }

    pub fn codes(result: &SqlValidationResult) -> Vec<SqlIssueCode> {
        result.issues.iter().map(|i| i.code).collect()
    }

    pub fn has_code(r: &SqlValidationResult, code: SqlIssueCode) -> bool {
        r.issues.iter().any(|i| i.code == code)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    /// The parser reads the default sql_mode and treats `/*!…*/` as a
    /// comment; a statement only another reading exposes is flagged, as an
    /// error under fail-closed tenant scoping (the app's run gate keys on the
    /// "tenant-scoped" phrase).
    #[test]
    fn readings_the_parser_did_not_see_are_flagged() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let options = SqlValidationOptions {
            dialect: Dialect::MySql,
            engine: Some(sqlly_lang::sqlmodel::DatabaseType::MySql),
            allow_multiple_statements: true,
            require_tenant_filter: true,
            fail_closed_tenant_filter: true,
            ..opts()
        };
        for sql in [
            r"SELECT * FROM Customers WHERE Name = 'a\'; DELETE FROM Customers; -- '",
            "/*!50000 DELETE FROM Customers */ SELECT 1",
        ] {
            let r = validate_sql(&store, sql, &options).unwrap();
            let issue = r
                .issues
                .iter()
                .find(|i| i.code == SqlIssueCode::LexicalAmbiguity)
                .unwrap_or_else(|| panic!("{sql}: {:?}", r.issues));
            assert_eq!(issue.severity, SqlIssueSeverity::Error, "{sql}");
            assert!(issue.message.contains("tenant-scoped"), "{sql}");
            assert!(!r.is_valid);
        }
        // A backslash-escaped quote with nothing hidden reads the same way.
        let r = validate_sql(
            &store,
            r"SELECT * FROM Customers WHERE Name = 'O\'Brien'",
            &options,
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::LexicalAmbiguity),
            "{:?}",
            r.issues
        );
        // Without an engine the check does not run.
        let r = validate_sql(
            &store,
            "/*!50000 DELETE FROM Customers */ SELECT 1",
            &SqlValidationOptions {
                dialect: Dialect::MySql,
                ..opts()
            },
        )
        .unwrap();
        assert!(!has_code(&r, SqlIssueCode::LexicalAmbiguity));
    }

    #[test]
    fn multiple_statements_is_error_by_default() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(
            &store,
            "SELECT 1 FROM dbo.Customers; SELECT 2 FROM dbo.Customers",
            &opts(),
        )
        .unwrap();
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::MultipleStatements)
        );
        assert!(!r.is_valid);
    }

    #[test]
    fn empty_sql_is_error() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "   ", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::EmptyStatement)
        );
    }

    #[test]
    fn multiple_statements_allowed_produces_no_issue() {
        // The editor validates whole script buffers; when multiple statements
        // are allowed their presence must not surface as an issue at all.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let options = SqlValidationOptions {
            allow_multiple_statements: true,
            ..Default::default()
        };
        let r = validate_sql(
            &store,
            "SELECT 1 FROM dbo.Customers; SELECT 2 FROM dbo.Customers",
            &options,
        )
        .unwrap();
        assert!(
            !has_code(&r, SqlIssueCode::MultipleStatements),
            "{:?}",
            r.issues
        );
        assert!(r.is_valid, "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_true_for_unresolved_table() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "SELECT * FROM dbo.Ghosts", &opts()).unwrap();
        assert!(r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_true_for_unresolved_column() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "SELECT c.Bogus FROM dbo.Customers AS c WHERE c.CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_for_clean_statement() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(
            &store,
            "SELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1",
            &opts(),
        )
        .unwrap();
        assert!(!r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_for_warning_only() {
        // Tenant-scope missing predicate is a warning, not a retryable error.
        let store = store_with(&[table("Orders", &["OrderId", "TenantId"])]);
        let r = validate_sql(&store, "SELECT OrderId FROM dbo.Orders", &opts()).unwrap();
        assert!(r.is_valid, "{:?}", r.issues);
        assert!(!r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_when_confirmation_required() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "DROP TABLE dbo.Customers", &opts()).unwrap();
        assert!(r.requires_confirmation);
        assert!(!r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_for_delete_without_where() {
        // Destructive-intent errors must NOT trigger an automatic rewrite that
        // would invent a predicate the user never asked for.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "DELETE FROM dbo.Customers", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DeleteWithoutWhere)
        );
        assert!(!r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_for_update_without_where() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let r = validate_sql(&store, "UPDATE dbo.Customers SET Name = 'x'", &opts()).unwrap();
        assert!(!r.is_valid);
        assert!(!r.has_only_retryable_errors(), "{:?}", r.issues);
    }

    #[test]
    fn has_only_retryable_errors_false_for_mixed_retryable_and_nonretryable() {
        // `DELETE FROM dbo.Ghosts` carries BOTH an unresolved-table error
        // (retryable) and a delete-without-where error (non-retryable). The
        // presence of any non-retryable blocking error must veto the retry, so
        // the loop never tries to "fix" the destructive statement.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let r = validate_sql(&store, "DELETE FROM dbo.Ghosts", &opts()).unwrap();
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::UnresolvedTable),
            "expected an unresolved-table error: {:?}",
            r.issues
        );
        assert!(
            r.issues
                .iter()
                .any(|i| i.code == SqlIssueCode::DeleteWithoutWhere),
            "expected a delete-without-where error: {:?}",
            r.issues
        );
        assert!(
            !r.has_only_retryable_errors(),
            "mixed retryable + non-retryable errors must not be retryable: {:?}",
            r.issues
        );
    }
}
