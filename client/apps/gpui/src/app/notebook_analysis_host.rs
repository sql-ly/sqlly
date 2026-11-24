//! Workbench side of the notebook analysis cells: runs their read-only
//! queries (related rows, plan captures, session counters, parameter
//! sweeps) on the cell's connection clone and hands the rows back to the
//! notebook. Nothing here goes through the results grid or the run gate:
//! every statement is built by the notebook from a cell whose SQL already
//! classified as read-only, and the profile carries the cell's read-only
//! mode.

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use super::*;
use crate::notebook::analysis::{
    AnalysisCancel, AnalysisReply, AnalysisRequest, AnalysisWork, QueryRows, StatsPair,
};
use crate::notebook::view::NotebookView;

type Failure = sqlly_core::DisplayMessage;

/// Rows of the first result set, at most `cap` (more rows set `truncated`).
async fn collect_rows(
    handle: sqlly_services::QueryHandle,
    cap: usize,
) -> Result<QueryRows, Failure> {
    let started = Instant::now();
    let mut events = handle.events;
    let mut out = QueryRows::default();
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::ResultSet { index: 0, columns } => out.columns = columns,
            sqlly_services::QueryEvent::Rows { index: 0, rows } => {
                for row in rows {
                    if out.rows.len() < cap {
                        out.rows.push(row);
                    } else {
                        out.truncated = true;
                    }
                }
            }
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                if let Some(set) = manifest.result_sets.first()
                    && let Some(all) = sqlly_services::take_spilled_rows(set)
                {
                    out.truncated = all.len() > cap;
                    out.rows = all.into_iter().take(cap).collect();
                }
                for set in manifest.result_sets.iter().skip(1) {
                    let _ = sqlly_services::take_spilled_rows(set);
                }
            }
            sqlly_services::QueryEvent::Error { message } => {
                return Err(Failure::raw(message));
            }
            sqlly_services::QueryEvent::Completed { .. } => break,
            sqlly_services::QueryEvent::Cancelled => {
                return Err(Failure::new(
                    "notebook.analysis.canceled",
                    "The query was canceled",
                ));
            }
            _ => {}
        }
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

fn canceled() -> Failure {
    Failure::new("notebook.analysis.canceled", "The query was canceled")
}

/// Record a started query with `cancel`; a cell stopped meanwhile gets the
/// query canceled at once.
async fn started(
    services: &Services,
    cancel: &AnalysisCancel,
    task_id: &str,
) -> Result<(), Failure> {
    if cancel.record(task_id) {
        return Ok(());
    }
    let _ = services.queries.cancel(task_id).await;
    Err(canceled())
}

async fn run_rows(
    services: &Services,
    profile: &ConnectionProfile,
    session: Option<&str>,
    sql: &str,
    cap: usize,
    cancel: &AnalysisCancel,
) -> Result<QueryRows, Failure> {
    if cancel.is_canceled() {
        return Err(canceled());
    }
    let handle = services
        .queries
        .execute_full_values(profile, session, sql, 256)
        .await
        .map_err(|e| e.message())?;
    let task_id = handle.task_id.clone();
    started(services, cancel, &task_id).await?;
    let rows = collect_rows(handle, cap).await;
    cancel.finished(&task_id);
    rows
}

async fn capture_plan(
    services: &Services,
    profile: &ConnectionProfile,
    session: Option<&str>,
    sql: &str,
    dialect: sqlly_services::plans::PlanDialect,
    cancel: &AnalysisCancel,
) -> Result<Vec<sqlly_services::plans::ExecutionPlan>, Failure> {
    if cancel.is_canceled() {
        return Err(canceled());
    }
    // Plan documents routinely run past the grid's 8 KiB cell preview.
    let handle = services
        .queries
        .execute_full_values(profile, session, sql, 64)
        .await
        .map_err(|e| e.message())?;
    let task_id = handle.task_id.clone();
    started(services, cancel, &task_id).await?;
    let (docs, server_error) = super::explain::collect_plan_documents(handle, dialect).await;
    cancel.finished(&task_id);
    if docs.is_empty() {
        return Err(match server_error {
            Some(message) => Failure::raw(message),
            None => Failure::new(
                "notebook.analysis.plan.none_returned",
                "The server returned no plan",
            ),
        });
    }
    let mut plans = Vec::new();
    for doc in &docs {
        plans.append(
            &mut sqlly_services::plans::parse_captured_plan(dialect, doc)
                .map_err(|e| Failure::raw(e.to_string()))?,
        );
    }
    Ok(plans)
}

const STATS_ROWS: usize = 64;

async fn work(
    services: Services,
    profile: ConnectionProfile,
    session: String,
    work: AnalysisWork,
    cancel: AnalysisCancel,
) -> AnalysisReply {
    let cancel = &cancel;
    let session = Some(session.as_str());
    match work {
        AnalysisWork::Rows { queries, max_rows } => {
            let mut sets = Vec::new();
            for sql in &queries {
                sets.push(run_rows(&services, &profile, None, sql, max_rows, cancel).await);
            }
            AnalysisReply::Rows(sets)
        }
        AnalysisWork::Plan {
            sql,
            actual,
            dialect,
            compare_sql,
            stats_sql,
        } => {
            let before = match (&stats_sql, actual) {
                (Some(s), true) => run_rows(&services, &profile, session, s, STATS_ROWS, cancel)
                    .await
                    .ok(),
                _ => None,
            };
            let plans = capture_plan(&services, &profile, session, &sql, dialect, cancel).await;
            let after = match (&stats_sql, &before) {
                (Some(s), Some(_)) => run_rows(&services, &profile, session, s, STATS_ROWS, cancel)
                    .await
                    .ok(),
                _ => None,
            };
            let compare = match &compare_sql {
                Some(c) => {
                    Some(capture_plan(&services, &profile, session, c, dialect, cancel).await)
                }
                None => None,
            };
            AnalysisReply::Plan {
                plans,
                compare,
                stats: before
                    .zip(after)
                    .map(|(before, after)| StatsPair { before, after }),
            }
        }
        AnalysisWork::Sweep { runs, stats_sql } => {
            let mut out = Vec::new();
            for sql in &runs {
                let before = match &stats_sql {
                    Some(s) => run_rows(&services, &profile, session, s, STATS_ROWS, cancel)
                        .await
                        .ok(),
                    None => None,
                };
                let timed = run_rows(&services, &profile, session, sql, 0, cancel).await;
                let after = match (&stats_sql, &before) {
                    (Some(s), Some(_)) => {
                        run_rows(&services, &profile, session, s, STATS_ROWS, cancel)
                            .await
                            .ok()
                    }
                    _ => None,
                };
                out.push(timed.map(|rows| {
                    (
                        rows.elapsed_ms,
                        before
                            .zip(after)
                            .map(|(before, after)| StatsPair { before, after }),
                    )
                }));
            }
            AnalysisReply::Sweep(out)
        }
    }
}

impl AppView {
    /// Run an analysis cell's queries and answer the notebook.
    pub(super) fn run_notebook_analysis(
        &mut self,
        notebook: Entity<NotebookView>,
        tab_idx: usize,
        request: AnalysisRequest,
        cx: &mut Context<Self>,
    ) {
        let request_id = request.request_id;
        let profile = match self.notebook_cell_profile_message(tab_idx, &request.target, cx) {
            Ok(profile) => profile,
            Err(message) => {
                notebook.update(cx, |nb, cx| {
                    nb.analysis_reply(request_id, AnalysisReply::Failed(message), cx)
                });
                return;
            }
        };
        let services = self.services_for_profile(&profile);
        let session = format!("notebook-analysis-{}", request.cell_id);
        let (tx, rx) = tokio::sync::oneshot::channel::<AnalysisReply>();
        let job = request.work;
        let cancel = AnalysisCancel::default();
        notebook.update(cx, |nb, _| {
            nb.track_analysis(&request.cell_id, cancel.clone());
            nb.route_analysis(&request.cell_id, services.clone());
        });
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let _ = tx.send(work(services, profile, session, job, cancel).await);
                })
                .await
                .ok();
        })
        .detach();
        let weak = notebook.downgrade();
        cx.spawn(async move |_this, cx| {
            let reply = rx.await.ok();
            weak.update(cx, |nb, cx| {
                let reply = reply.unwrap_or_else(|| AnalysisReply::Failed(canceled()));
                nb.analysis_reply(request_id, reply, cx)
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_core::{CellValue, ResultColumn};
    use sqlly_services::plans::PlanDialect;
    use sqlly_services::{QueryEvent, QueryExecutor, QueryHandle};
    use std::sync::Mutex;

    /// A server answering every query with one plan document. Grid runs cut
    /// it to the 8 KiB cell preview, as every executor's grid policy does;
    /// only the full-values path sends it whole.
    struct PlanServer {
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        asked: Mutex<Vec<&'static str>>,
    }

    impl PlanServer {
        fn new(columns: &[&str], row: Vec<CellValue>) -> Arc<Self> {
            Arc::new(Self {
                columns: columns
                    .iter()
                    .map(|name| ResultColumn {
                        name: name.to_string(),
                        data_type: "nvarchar".into(),
                        nullable: true,
                    })
                    .collect(),
                row,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn reply(&self, path: &'static str, full: bool) -> QueryHandle {
            self.asked.lock().unwrap().push(path);
            let mut rows = vec![self.row.clone()];
            let cut = if full {
                Vec::new()
            } else {
                sqlly_services::truncate_rows(&mut rows, 0, &sqlly_services::CellLimits::default())
            };
            let mut events = vec![
                QueryEvent::Started,
                QueryEvent::ResultSet {
                    index: 0,
                    columns: self.columns.clone(),
                },
                QueryEvent::Rows { index: 0, rows },
            ];
            if !cut.is_empty() {
                events.push(QueryEvent::TruncatedCells {
                    index: 0,
                    cells: cut,
                });
            }
            events.push(QueryEvent::Completed { elapsed_ms: 1 });
            let (tx, rx) = tokio::sync::mpsc::channel(events.len());
            for event in events {
                tx.try_send(event).unwrap();
            }
            QueryHandle {
                task_id: "plan-1".into(),
                events: rx,
            }
        }
    }

    #[async_trait::async_trait]
    impl QueryExecutor for PlanServer {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<QueryHandle> {
            Ok(self.reply("grid", false))
        }

        async fn execute_full_values(
            &self,
            _profile: &ConnectionProfile,
            _session: Option<&str>,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<QueryHandle> {
            Ok(self.reply("full", true))
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            Ok(())
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "Sales",
            "db.example",
            "reader",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "plan-test".into(),
            },
        )
    }

    fn capture(server: Arc<PlanServer>, dialect: PlanDialect) -> AnalysisReply {
        crate::ensure_test_tokio_runtime();
        let mut services = Services::new();
        services.queries = server;
        let sql = sqlly_services::plans::explain_sql_with_options(
            dialect,
            &sqlly_services::plans::ExplainOptions::estimated_defaults(),
            false,
            "SELECT * FROM dbo.Orders",
        );
        crate::tokio_handle().block_on(work(
            services,
            profile(),
            "notebook-analysis-plan".into(),
            AnalysisWork::Plan {
                sql,
                actual: false,
                dialect,
                compare_sql: None,
                stats_sql: None,
            },
            AnalysisCancel::default(),
        ))
    }

    /// The statement text a plan carries, long enough that the document is
    /// well past 8 KiB.
    fn long_statement() -> String {
        format!(
            "SELECT {} FROM dbo.Orders",
            (0..3000)
                .map(|i| format!("c{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    fn statement_text(reply: AnalysisReply) -> String {
        let AnalysisReply::Plan { plans, .. } = reply else {
            panic!("a plan reply")
        };
        let plans = plans.unwrap_or_else(|e| panic!("the plan parses: {e:?}"));
        plans[0].statement_text.clone()
    }

    #[test]
    fn a_showplan_larger_than_the_cell_preview_reaches_the_parser_whole() {
        let statement = long_statement();
        let xml = format!(
            r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements><StmtSimple StatementText="{statement}" StatementType="SELECT" StatementSubTreeCost="2"><QueryPlan><RelOp NodeId="0" LogicalOp="Table Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="2"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" /></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#
        );
        assert!(xml.len() > 2 * 8 * 1024, "{}", xml.len());
        let server = PlanServer::new(
            &["Microsoft SQL Server 2005 XML Showplan"],
            vec![CellValue::String(xml)],
        );
        let reply = capture(server.clone(), PlanDialect::SqlServer);
        assert_eq!(statement_text(reply), statement);
        assert_eq!(*server.asked.lock().unwrap(), ["full"]);
    }

    #[test]
    fn a_json_plan_larger_than_the_cell_preview_reaches_the_parser_whole() {
        let filter = (0..1500)
            .map(|i| format!("(c{i} > 0)"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let json = format!(
            r#"[{{"Plan": {{"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "orders", "Startup Cost": 0.0, "Total Cost": 12.5, "Plan Rows": 40, "Plan Width": 8, "Filter": "{filter}"}}}}]"#
        );
        assert!(json.len() > 2 * 8 * 1024, "{}", json.len());
        let server = PlanServer::new(&["QUERY PLAN"], vec![CellValue::String(json)]);
        let reply = capture(server.clone(), PlanDialect::Postgres);
        assert_eq!(*server.asked.lock().unwrap(), ["full"]);
        let AnalysisReply::Plan { plans, .. } = reply else {
            panic!("a plan reply")
        };
        let plans = plans.unwrap_or_else(|e| panic!("the plan parses: {e:?}"));
        assert!(!plans[0].nodes.is_empty());
    }
}
