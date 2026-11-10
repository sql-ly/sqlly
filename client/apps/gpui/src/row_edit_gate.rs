//! The synchronous "can this result row be edited?" decision.
//!
//! Opening the row editor runs the authoritative check on the server
//! (provenance, primary key, the row still existing), but several refusals are
//! known before any round trip: no connection, an engine without row editing,
//! a SELECT-only or auto-rollback connection policy, a later result set of a
//! batch, restored historical results, or a query that is not a simple
//! single-table `SELECT`. The results grid uses [`edit_block_reason`] to hide
//! its row-header pencil wherever one of those refusals is certain, and the
//! click path uses the same pieces, so the pencil never offers an edit the
//! app would turn down before asking the server.

use sqlly_core::ConnectionProfile;

pub(crate) fn localized_not_connected() -> String {
    crate::localization::text("results.editing.connect_to_edit_rows")
}

/// Connection-level refusal used by the click path before the editor opens:
/// no connection, or an engine without row editing.
pub(crate) fn connection_block_reason(profile: Option<&ConnectionProfile>) -> Option<String> {
    let Some(profile) = profile else {
        return Some(localized_not_connected());
    };
    crate::capabilities::row_editing(profile.database_type).reason()
}

/// [`connection_block_reason`] plus the connection-policy gate the row-editing
/// service applies when a session opens (SELECT-only, auto-rollback).
/// `profile` must already carry the inherited execution policy
/// (`mutation_review::apply_inherited_execution_policy`), exactly like the
/// profile the editor opens with.
pub(crate) fn effective_connection_block_reason(
    profile: Option<&ConnectionProfile>,
) -> Option<String> {
    connection_block_reason(profile).or_else(|| {
        profile
            .and_then(sqlly_services::row_editing::policy_block_message)
            .map(crate::localization::message)
    })
}

/// What the grid knows about one result set when deciding whether to show
/// the Edit (pencil) row action.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResultEditContext<'a> {
    /// The results were restored from query history, not run live.
    pub historical: bool,
    /// The SQL that produced the results.
    pub sql: Option<&'a str>,
    /// The result set's position in its batch (0 = first).
    pub result_set_index: usize,
    /// The connection-level verdict from [`effective_connection_block_reason`]:
    /// `None` when the view was never told (keep the pencil — the click path
    /// still explains), `Some(None)` editable, `Some(Some(reason))` refused.
    pub connection_block: Option<Option<&'a str>>,
}

/// Why the rows of this result set cannot be edited, or `None` when the
/// pencil should show. Per-row refusals that need the table's key metadata
/// (a NULL key value, a deleted row) are still left to the editor.
pub(crate) fn edit_block_reason(ctx: ResultEditContext<'_>) -> Option<String> {
    if ctx.historical {
        return Some(crate::localization::text(
            "results.row_edit_gate.historical_results",
        ));
    }
    if let Some(Some(reason)) = ctx.connection_block {
        return Some(reason.to_string());
    }
    if ctx.result_set_index != 0 {
        // Mirrors the row-editing service: provenance is only available for
        // the first result set of a batch.
        return Some(crate::localization::text(
            "results.row_edit_gate.later_result_set",
        ));
    }
    let Some(sql) = ctx.sql else {
        return Some(crate::localization::text(
            "results.row_edit_gate.missing_query",
        ));
    };
    if crate::column_layout::single_table_target(sql).is_none() {
        return Some(crate::localization::text(
            "results.row_edit_gate.not_single_table_select",
        ));
    }
    if projection_is_aggregate_only(sql) {
        return Some(crate::localization::text(
            "results.row_edit_gate.aggregate_rows",
        ));
    }
    None
}

/// Aggregate functions whose plain (non-windowed) call collapses rows.
const AGGREGATES: &[&str] = &[
    "count",
    "count_big",
    "sum",
    "avg",
    "min",
    "max",
    "string_agg",
    "group_concat",
    "array_agg",
    "listagg",
    "stdev",
    "stdevp",
    "stddev",
    "var",
    "varp",
    "variance",
    "bool_and",
    "bool_or",
    "json_agg",
    "jsonb_agg",
];

/// True when every item of the `SELECT` list is a plain aggregate call (no
/// `OVER`), e.g. `SELECT count(*) FROM t`. Such a result projects no table
/// column at all, so no key can identify a row and the editor would always
/// refuse it — on every engine.
fn projection_is_aggregate_only(sql: &str) -> bool {
    use sqlly_intel::query::tokenizer::{TokenKind, tokenize};
    let tokens: Vec<_> = tokenize(sql)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect();
    let is_word = |t: &sqlly_intel::query::tokenizer::Token<'_>| {
        matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
    };
    let Some(first) = tokens.first() else {
        return false;
    };
    if !(is_word(first) && first.text.eq_ignore_ascii_case("select")) {
        return false;
    }
    // Split the SELECT list (up to the top-level FROM) on top-level commas.
    let mut items: Vec<Vec<&sqlly_intel::query::tokenizer::Token<'_>>> = vec![Vec::new()];
    let mut depth = 0usize;
    let mut saw_from = false;
    for token in &tokens[1..] {
        if token.kind == TokenKind::Symbol {
            match token.text {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                "," if depth == 0 => {
                    items.push(Vec::new());
                    continue;
                }
                _ => {}
            }
        } else if depth == 0 && is_word(token) && token.text.eq_ignore_ascii_case("from") {
            saw_from = true;
            break;
        }
        items.last_mut().expect("never empty").push(token);
    }
    if !saw_from {
        return false;
    }
    items.iter().all(|item| {
        // `DISTINCT` / `ALL` / `TOP n` prefixes belong to the first item.
        let mut rest: &[&sqlly_intel::query::tokenizer::Token<'_>] = item;
        while let Some((head, tail)) = rest.split_first() {
            if is_word(head)
                && (head.text.eq_ignore_ascii_case("distinct")
                    || head.text.eq_ignore_ascii_case("all"))
            {
                rest = tail;
            } else if is_word(head) && head.text.eq_ignore_ascii_case("top") {
                // Skip `TOP n` / `TOP (n)` and an optional `PERCENT`.
                let mut skip = 2;
                if tail.first().is_some_and(|t| t.text == "(") {
                    skip = tail.iter().position(|t| t.text == ")").map_or(2, |p| p + 2);
                }
                rest = rest.get(skip..).unwrap_or(&[]);
                if rest
                    .first()
                    .is_some_and(|t| is_word(t) && t.text.eq_ignore_ascii_case("percent"))
                {
                    rest = &rest[1..];
                }
            } else {
                break;
            }
        }
        let starts_with_aggregate = match rest {
            [name, open, ..] => {
                is_word(name)
                    && AGGREGATES.iter().any(|a| name.text.eq_ignore_ascii_case(a))
                    && open.kind == TokenKind::Symbol
                    && open.text == "("
            }
            _ => false,
        };
        let windowed = rest
            .iter()
            .any(|t| is_word(t) && t.text.eq_ignore_ascii_case("over"));
        starts_with_aggregate && !windowed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, SecretProvider, SecretRef};

    fn ctx(sql: &str) -> ResultEditContext<'_> {
        ResultEditContext {
            historical: false,
            sql: Some(sql),
            result_set_index: 0,
            connection_block: Some(None),
        }
    }

    fn profile(db: DatabaseType) -> ConnectionProfile {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        ConnectionProfile {
            database_type: db,
            ..ConnectionProfile::sql_login("test", "host", "sa", secret)
        }
    }

    #[test]
    fn simple_single_table_select_is_editable() {
        assert_eq!(edit_block_reason(ctx("SELECT * FROM dbo.Customers")), None);
    }

    #[test]
    fn joins_derived_tables_and_non_selects_are_not_editable() {
        for sql in [
            "SELECT * FROM a JOIN b ON a.id = b.a_id",
            "SELECT x FROM (SELECT 1 AS x) d",
            "UPDATE dbo.Customers SET name = 'x'",
        ] {
            assert!(edit_block_reason(ctx(sql)).is_some(), "{sql}");
        }
    }

    #[test]
    fn aggregate_only_projections_are_not_editable() {
        for sql in [
            "SELECT count(*) FROM dbo.Customers",
            "SELECT COUNT(*), MAX(id) FROM dbo.Customers WHERE x = 1",
            "SELECT TOP 10 sum(total) AS t FROM dbo.Orders",
            "select distinct avg(price) from products",
        ] {
            assert!(edit_block_reason(ctx(sql)).is_some(), "{sql}");
        }
    }

    #[test]
    fn projections_with_a_table_column_stay_editable() {
        for sql in [
            "SELECT id, count(*) OVER () FROM dbo.Customers",
            "SELECT count(*) OVER (), id FROM dbo.Customers",
            "SELECT id, name FROM dbo.Customers WHERE name LIKE 'a%'",
            "SELECT TOP (5) * FROM dbo.Customers",
            "SELECT maxval FROM dbo.Limits",
        ] {
            assert_eq!(edit_block_reason(ctx(sql)), None, "{sql}");
        }
    }

    #[test]
    fn later_result_sets_are_not_editable() {
        let c = ResultEditContext {
            result_set_index: 1,
            ..ctx("SELECT * FROM dbo.Customers")
        };
        assert!(edit_block_reason(c).is_some());
    }

    #[test]
    fn historical_results_are_not_editable() {
        let c = ResultEditContext {
            historical: true,
            ..ctx("SELECT * FROM dbo.Customers")
        };
        assert!(edit_block_reason(c).is_some());
    }

    #[test]
    fn missing_sql_is_not_editable() {
        let c = ResultEditContext {
            sql: None,
            ..ctx("SELECT 1")
        };
        assert!(edit_block_reason(c).is_some());
    }

    #[test]
    fn connection_refusal_hides_the_pencil_but_unknown_does_not() {
        let blocked = ResultEditContext {
            connection_block: Some(Some("read only")),
            ..ctx("SELECT * FROM dbo.Customers")
        };
        assert_eq!(edit_block_reason(blocked).as_deref(), Some("read only"));
        let unknown = ResultEditContext {
            connection_block: None,
            ..ctx("SELECT * FROM dbo.Customers")
        };
        assert_eq!(edit_block_reason(unknown), None);
    }

    #[test]
    fn no_connection_blocks() {
        assert_eq!(
            connection_block_reason(None).as_deref(),
            Some(localized_not_connected().as_str())
        );
        assert!(effective_connection_block_reason(None).is_some());
    }

    #[test]
    fn engines_without_row_editing_block() {
        assert!(connection_block_reason(Some(&profile(DatabaseType::SqlServer))).is_none());
        assert!(connection_block_reason(Some(&profile(DatabaseType::Redis))).is_some());
        assert!(connection_block_reason(Some(&profile(DatabaseType::ClickHouse))).is_some());
    }

    #[test]
    fn select_only_and_auto_rollback_policies_block() {
        let ok = profile(DatabaseType::SqlServer);
        assert!(effective_connection_block_reason(Some(&ok)).is_none());
        let select_only = ConnectionProfile {
            block_all_but_select: true,
            ..ok.clone()
        };
        assert!(effective_connection_block_reason(Some(&select_only)).is_some());
        let rollback = ConnectionProfile {
            auto_wrap_rollback: true,
            ..ok
        };
        assert!(effective_connection_block_reason(Some(&rollback)).is_some());
    }
}
