//! Pre-flight impact estimates for MCP write proposals: row count and a
//! one-line plan summary, bounded to 5 s per statement.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, ResultColumn};
use sqlly_services::plans::PlanDialect;
use sqlly_services::{QueryEvent, Services, plan_dialect};

use crate::mutation_review::{self, ChangeType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    pub estimated_rows: Option<u64>,
    pub plan_summary: Option<String>,
    pub note: Option<String>,
}

/// COUNT(*) SQL for a single-table UPDATE/DELETE, else `None`. The proposal
/// must read as exactly one statement for `db` (a batch, or text the engine
/// could read more than one way, gets no estimate), so the table and WHERE
/// text lifted from it are the statement the engine would run.
pub fn count_sql(db: DatabaseType, sql: &str) -> Option<String> {
    if !sqlly_intel::query::safety::is_single_statement_for(db, sql) {
        return None;
    }
    let analysis = mutation_review::analyze_for(db, sql)?;
    if analysis.ambiguity.is_some() || analysis.statements.len() != 1 {
        return None;
    }
    let first = analysis.statements.first()?;
    if !matches!(first.change_type, ChangeType::Update | ChangeType::Delete) {
        return None;
    }
    let table = extract_single_table(sql)?;
    let lower = first_statement(sql)?.to_ascii_lowercase();
    if lower.contains(" join ") || (lower.contains("update") && lower.contains(" from ")) {
        return None;
    }
    if first.is_unbounded_dangerous() {
        Some(format!("SELECT COUNT(*) FROM {table}"))
    } else if let Some(where_clause) = extract_where_clause(sql) {
        Some(format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}"))
    } else {
        Some(format!("SELECT COUNT(*) FROM {table}"))
    }
}

pub fn explain_sql(db: DatabaseType, sql: &str) -> Option<String> {
    let dialect = plan_dialect(db)?;
    Some(sqlly_services::plans::estimated_capture_sql(dialect, sql))
}

pub fn summarise_plan(
    db: DatabaseType,
    _columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> Option<String> {
    let dialect = plan_dialect(db)?;
    let first_row = rows.first()?;
    match dialect {
        PlanDialect::Postgres | PlanDialect::MySql | PlanDialect::MariaDb => {
            let text = first_row.first().and_then(|cell| match cell {
                CellValue::String(s) => Some(s.as_str()),
                _ => None,
            })?;
            let value: serde_json::Value = serde_json::from_str(text).ok()?;
            let plan = value
                .pointer("/0/Plan")
                .or_else(|| value.pointer("/Plan"))
                .or_else(|| value.get("query_block"))?;
            let node_type = plan
                .get("Node Type")
                .or_else(|| plan.get("select_type"))
                .and_then(|v| v.as_str())?;
            let rows_est = plan
                .get("Plan Rows")
                .or_else(|| plan.get("rows"))
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n as u64)));
            Some(match rows_est {
                Some(n) => crate::localization::format(
                    "mcp.preflight.plan_rows",
                    &[("node_type", node_type), ("rows", &n.to_string())],
                ),
                None => node_type.to_string(),
            })
        }
        PlanDialect::SqlServer => {
            let text = first_row.first().and_then(|cell| match cell {
                CellValue::String(s) => Some(s.as_str()),
                _ => None,
            })?;
            let op = text
                .split("StatementOptmLevel")
                .next()
                .and_then(|chunk| chunk.rsplit('>').nth(1))
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            Some(op.to_string())
        }
        PlanDialect::Sqlite => {
            let detail = first_row.get(3).or_else(|| first_row.get(2))?;
            match detail {
                CellValue::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            }
        }
        // One `explain` column holding a PostgreSQL-shaped step tree with no
        // cardinality estimates: name the root step.
        PlanDialect::ClickHouse => {
            let text = first_row.first().and_then(|cell| match cell {
                CellValue::String(s) => Some(s.as_str()),
                _ => None,
            })?;
            let value: serde_json::Value = serde_json::from_str(text).ok()?;
            let plan = value
                .pointer("/0/Plan")
                .or_else(|| value.pointer("/Plan"))?;
            let node_type = plan.get("Node Type").and_then(|v| v.as_str())?;
            Some(match plan.get("Description").and_then(|v| v.as_str()) {
                Some(description) if !description.is_empty() => {
                    format!("{node_type} · {description}")
                }
                _ => node_type.to_string(),
            })
        }
        // One tab-separated PLAN_TABLE row: id, parent, operation, options,
        // owner, object, cardinality, …
        PlanDialect::Oracle => {
            let text = first_row.first().and_then(|cell| match cell {
                CellValue::String(s) => Some(s.as_str()),
                _ => None,
            })?;
            let fields: Vec<&str> = text.split('\t').collect();
            let operation = fields.get(2).map(|s| s.trim()).filter(|s| !s.is_empty())?;
            let options = fields.get(3).map(|s| s.trim()).unwrap_or("");
            let node_type = if options.is_empty() {
                operation.to_string()
            } else {
                format!("{operation} {options}")
            };
            let rows_est = fields.get(6).and_then(|s| s.trim().parse::<u64>().ok());
            Some(match rows_est {
                Some(n) => crate::localization::format(
                    "mcp.preflight.plan_rows",
                    &[("node_type", &node_type), ("rows", &n.to_string())],
                ),
                None => node_type,
            })
        }
        // `(explain_key, explain_value)`: the value is a JSON array whose
        // root operator carries an estimated cardinality.
        PlanDialect::DuckDb => {
            let text = first_row.get(1).and_then(|cell| match cell {
                CellValue::String(s) => Some(s.as_str()),
                _ => None,
            })?;
            let value: serde_json::Value = serde_json::from_str(text).ok()?;
            let root = value.get(0).unwrap_or(&value);
            let node_type = root.get("name").and_then(|v| v.as_str())?;
            let rows_est = root
                .pointer("/extra_info/Estimated Cardinality")
                .and_then(|v| v.as_str())
                .and_then(|s| s.trim().parse::<u64>().ok());
            Some(match rows_est {
                Some(n) => crate::localization::format(
                    "mcp.preflight.plan_rows",
                    &[("node_type", node_type), ("rows", &n.to_string())],
                ),
                None => node_type.to_string(),
            })
        }
    }
}

pub fn preflight_summary(preflight: &Preflight) -> String {
    if let Some(note) = &preflight.note {
        return crate::localization::format(
            "mcp.preflight.estimate_unavailable_reason",
            &[("reason", note)],
        );
    }
    match (&preflight.estimated_rows, &preflight.plan_summary) {
        (Some(rows), Some(plan)) => crate::localization::format(
            "mcp.preflight.estimated_rows_with_plan",
            &[("rows", &rows.to_string()), ("plan", plan)],
        ),
        (Some(rows), None) => crate::localization::format(
            "mcp.preflight.estimated_rows",
            &[("rows", &rows.to_string())],
        ),
        (None, Some(plan)) => plan.clone(),
        (None, None) => crate::localization::text("mcp.preflight.estimate_unavailable"),
    }
}

pub async fn run_preflight(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> Preflight {
    let mut preflight = Preflight {
        estimated_rows: None,
        plan_summary: None,
        note: None,
    };
    // The proposal comes from an MCP client and these queries run before
    // anyone has approved it. The COUNT is composed from pieces of that text,
    // so it runs behind the SELECT-only gate and the engine's read-only mode:
    // a statement smuggled into the lifted WHERE text (`… WHERE 1=1 DROP
    // TABLE x` needs no separator on SQL Server) is refused, not run.
    let db = profile.database_type;
    let mut count_profile = profile.clone();
    count_profile.block_all_but_select = true;
    count_profile.enforce_read_only = true;
    if let Some(count) = count_sql(db, sql) {
        match run_bounded_query(services, &count_profile, &count).await {
            Ok(outcome) => {
                preflight.estimated_rows = outcome
                    .rows
                    .first()
                    .and_then(|row| row.first())
                    .and_then(|cell| match cell {
                        CellValue::I64(n) => Some(*n as u64),
                        CellValue::F64(n) => Some(*n as u64),
                        CellValue::String(s) => s.parse().ok(),
                        _ => None,
                    });
            }
            Err(message) => preflight.note = Some(message),
        }
    }
    // An estimated plan never executes the statement it explains, but a
    // second statement riding along would run: explain only a proposal that
    // is exactly one statement. SQL Server's SHOWPLAN mode and Oracle's
    // EXPLAIN PLAN compile without executing (and cannot run inside a
    // read-only wrap); everywhere else the capture also runs read-only.
    let explain = sqlly_intel::query::safety::is_single_statement_for(db, sql)
        .then(|| explain_sql(db, sql))
        .flatten();
    let mut explain_profile = profile.clone();
    explain_profile.enforce_read_only = !matches!(
        db.connection_kind(),
        sqlly_core::ConnectionKind::Tds | sqlly_core::ConnectionKind::OracleNet
    );
    if let Some(explain) = explain {
        match run_bounded_query(services, &explain_profile, &explain).await {
            Ok(outcome) => {
                if let Some(summary) =
                    summarise_plan(profile.database_type, &outcome.columns, &outcome.rows)
                {
                    preflight.plan_summary = Some(summary);
                }
            }
            Err(message) => {
                if preflight.note.is_none() {
                    preflight.note = Some(message);
                }
            }
        }
    }
    preflight
}

struct QueryOutcome {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
}

async fn run_bounded_query(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> Result<QueryOutcome, String> {
    let work = async {
        let mut handle = services
            .queries
            .execute(profile, sql, 64)
            .await
            .map_err(|error| crate::localization::message(error.message()))?;
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        while let Some(event) = handle.events.recv().await {
            match event {
                QueryEvent::ResultSet {
                    index,
                    columns: cols,
                } => {
                    if index == 0 {
                        columns = cols;
                    }
                }
                QueryEvent::Rows { index, rows: batch } => {
                    if index == 0 {
                        rows.extend(batch);
                    }
                }
                QueryEvent::Error { message } => return Err(message),
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }
        Ok(QueryOutcome { columns, rows })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), work)
        .await
        .map_err(|_| crate::localization::text("mcp.preflight.query_timeout"))?
}

fn extract_single_table(sql: &str) -> Option<String> {
    let statement = first_statement(sql)?;
    let lower = statement.to_ascii_lowercase();
    if let Some(idx) = lower.find("update ") {
        let rest = statement[idx + 7..].trim_start();
        return Some(take_table_identifier(rest));
    }
    if let Some(idx) = lower.find("delete from ") {
        let rest = statement[idx + 12..].trim_start();
        return Some(take_table_identifier(rest));
    }
    None
}

fn extract_where_clause(sql: &str) -> Option<String> {
    let statement = first_statement(sql)?;
    let lower = statement.to_ascii_lowercase();
    let idx = lower.rfind(" where ")?;
    let clause = statement[idx + 7..].trim();
    if clause.is_empty() {
        None
    } else {
        Some(clause.to_string())
    }
}

fn first_statement(sql: &str) -> Option<&str> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = trimmed.find(';').unwrap_or(trimmed.len());
    Some(trimmed[..end].trim())
}

fn take_table_identifier(rest: &str) -> String {
    let mut out = String::new();
    for ch in rest.chars() {
        if ch.is_whitespace() || ch == ';' {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Preflight queries are composed from MCP-proposed text and run before
    /// approval: a multi-statement proposal gets no COUNT, and a statement
    /// smuggled into the lifted WHERE text (no separator needed on SQL
    /// Server) leaves a COUNT that the SELECT-only gate it runs under refuses.
    #[test]
    fn preflight_counts_cannot_carry_a_second_statement() {
        assert_eq!(
            count_sql(
                DatabaseType::Postgres,
                "DELETE FROM t WHERE 1 = 1; DROP TABLE secrets"
            ),
            None
        );
        assert_eq!(
            count_sql(
                DatabaseType::MySql,
                "DELETE FROM t WHERE a = `'`; DROP TABLE secrets; SELECT `'`"
            ),
            None
        );
        let smuggled = count_sql(
            DatabaseType::SqlServer,
            "DELETE FROM t WHERE 1 = 1 DROP TABLE secrets",
        )
        .expect("one `;`-statement");
        assert!(
            !sqlly_intel::query::safety::classify_select_only_for(
                DatabaseType::SqlServer,
                &smuggled
            )
            .select_only,
            "{smuggled}"
        );
    }

    #[test]
    fn count_sql_single_table_update_with_where() {
        assert_eq!(
            count_sql(
                DatabaseType::SqlServer,
                "UPDATE dbo.Orders SET x = 1 WHERE id = 5"
            ),
            Some("SELECT COUNT(*) FROM dbo.Orders WHERE id = 5".to_string())
        );
    }

    #[test]
    fn count_sql_delete_without_where_counts_whole_table() {
        assert_eq!(
            count_sql(DatabaseType::SqlServer, "DELETE FROM dbo.Orders"),
            Some("SELECT COUNT(*) FROM dbo.Orders".to_string())
        );
    }

    #[test]
    fn count_sql_none_for_join_or_insert() {
        assert_eq!(
            count_sql(DatabaseType::SqlServer, "INSERT INTO t VALUES (1)"),
            None
        );
        assert_eq!(
            count_sql(
                DatabaseType::SqlServer,
                "UPDATE a SET x = 1 FROM a JOIN b ON a.id = b.id"
            ),
            None
        );
    }

    #[test]
    fn explain_sql_per_dialect() {
        assert!(
            explain_sql(DatabaseType::SqlServer, "SELECT 1")
                .unwrap()
                .contains("SHOWPLAN")
        );
        assert!(
            explain_sql(DatabaseType::Postgres, "SELECT 1")
                .unwrap()
                .contains("EXPLAIN")
        );
        assert!(
            explain_sql(DatabaseType::MySql, "SELECT 1")
                .unwrap()
                .contains("EXPLAIN")
        );
        assert!(
            explain_sql(DatabaseType::Sqlite, "SELECT 1")
                .unwrap()
                .contains("EXPLAIN QUERY PLAN")
        );
    }

    #[test]
    fn summarise_pg_json_plan() {
        let columns = vec![ResultColumn {
            name: "plan".into(),
            data_type: "json".into(),
            nullable: true,
        }];
        let rows = vec![vec![CellValue::String(
            r#"[{"Plan":{"Node Type":"Seq Scan","Plan Rows":1234}}]"#.into(),
        )]];
        assert_eq!(
            summarise_plan(DatabaseType::Postgres, &columns, &rows),
            Some("Seq Scan · ~1234 rows".to_string())
        );
    }

    #[test]
    fn summarise_unexpected_shape_is_none() {
        let columns = vec![ResultColumn {
            name: "plan".into(),
            data_type: "json".into(),
            nullable: true,
        }];
        let rows = vec![vec![CellValue::String("{}".into())]];
        assert_eq!(
            summarise_plan(DatabaseType::Postgres, &columns, &rows),
            None
        );
    }
}
