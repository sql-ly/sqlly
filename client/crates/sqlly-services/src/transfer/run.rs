//! Transfer runner: stream rows from the source query port into the sink.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use sqlly_core::{CellValue, ConnectionProfile, SqlParam};

use crate::query::QueryEvent;
use crate::services::Services;
use crate::transfer::plan::{TransferPlan, insert_sql, ordered_tables, select_sql, statements_for};
use crate::transfer::sink::TransferSink;

#[derive(Debug, Clone)]
pub enum TransferProgress {
    TableStarted {
        table: String,
        index: usize,
        total: usize,
    },
    Rows {
        table: String,
        copied: u64,
    },
    TableFinished {
        table: String,
        copied: u64,
    },
    Warning(sqlly_core::DisplayMessage),
    Finished(TransferOutcome),
}

#[derive(Debug, Clone, Default)]
pub struct TransferOutcome {
    pub tables: Vec<(String, u64)>,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
    pub error: Option<sqlly_core::DisplayMessage>,
    pub cancelled: bool,
}

#[derive(Clone, Default)]
pub struct TransferCancel(Arc<AtomicBool>);

impl TransferCancel {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn table_label(table: &crate::transfer::plan::TransferTable) -> String {
    match &table.schema {
        Some(schema) if !schema.is_empty() => format!("{schema}.{}", table.name),
        _ => table.name.clone(),
    }
}

fn transfer_error(table: &str, error: impl std::fmt::Display) -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "services.transfer.errors.table_failed",
        format!("{table}: {error}"),
    )
    .with_arg("table_name", table)
    .with_arg("detail", error.to_string())
}

fn cell_to_param(
    value: &CellValue,
    warnings: &mut Vec<sqlly_core::DisplayMessage>,
    warned: &mut bool,
) -> SqlParam {
    match value {
        CellValue::F64(v) if !v.is_finite() => {
            if !*warned {
                warnings.push(sqlly_core::DisplayMessage::new(
                    "services.transfer.warnings.non_finite_null",
                    "non-finite floating-point values were sent as NULL",
                ));
                *warned = true;
            }
            SqlParam::Null
        }
        other => SqlParam::from(other),
    }
}

pub async fn run_transfer(
    source: &Services,
    source_profile: &ConnectionProfile,
    sink: &mut dyn TransferSink,
    plan: &TransferPlan,
    progress: tokio::sync::mpsc::Sender<TransferProgress>,
    cancel: TransferCancel,
) -> TransferOutcome {
    let mut outcome = TransferOutcome::default();
    let (tables, order_warnings) = ordered_tables(plan);
    outcome.warnings.extend(order_warnings);
    let total = tables.len();

    for (index, table) in tables.into_iter().enumerate() {
        let label = table_label(table);
        let _ = progress
            .send(TransferProgress::TableStarted {
                table: label.clone(),
                index,
                total,
            })
            .await;

        let stmts = match statements_for(plan, table) {
            Ok(stmts) => stmts,
            Err(message) => {
                outcome.warnings.push(message.clone());
                let _ = progress.send(TransferProgress::Warning(message)).await;
                continue;
            }
        };
        outcome.warnings.extend(stmts.warnings.clone());

        if let Err(err) = sink.begin().await {
            outcome.error = Some(transfer_error(&label, err));
            let _ = progress
                .send(TransferProgress::Finished(outcome.clone()))
                .await;
            return outcome;
        }

        for sql in &stmts.pre {
            if let Err(err) = sink.execute(sql, &[]).await {
                let _ = sink.rollback().await;
                outcome.error = Some(transfer_error(&label, err));
                let _ = progress
                    .send(TransferProgress::Finished(outcome.clone()))
                    .await;
                return outcome;
            }
        }

        let select = select_sql(plan, table);
        // Full values: the target must receive every value whole, never the
        // 8 KiB preview a grid paints.
        let handle = match source
            .queries
            .execute_full_values(source_profile, None, &select, 256)
            .await
        {
            Ok(handle) => handle,
            Err(err) => {
                let _ = sink.rollback().await;
                outcome.error = Some(transfer_error(&label, err));
                let _ = progress
                    .send(TransferProgress::Finished(outcome.clone()))
                    .await;
                return outcome;
            }
        };

        let mut buffer: Vec<Vec<CellValue>> = Vec::new();
        let mut copied: u64 = 0;
        let batch_size = stmts.rows_per_statement;
        let mut non_finite_warned = false;
        let mut events = handle.events;

        loop {
            let event = events.recv().await;
            let Some(event) = event else {
                break;
            };
            match event {
                QueryEvent::Rows { rows, .. } => {
                    if cancel.is_requested() {
                        let _ = source.queries.cancel(&handle.task_id).await;
                        let _ = sink.rollback().await;
                        outcome.cancelled = true;
                        let _ = progress
                            .send(TransferProgress::Finished(outcome.clone()))
                            .await;
                        return outcome;
                    }
                    buffer.extend(rows);
                    while buffer.len() >= batch_size {
                        if cancel.is_requested() {
                            let _ = source.queries.cancel(&handle.task_id).await;
                            let _ = sink.rollback().await;
                            outcome.cancelled = true;
                            let _ = progress
                                .send(TransferProgress::Finished(outcome.clone()))
                                .await;
                            return outcome;
                        }
                        let chunk: Vec<_> = buffer.drain(..batch_size).collect();
                        let sql = insert_sql(plan, table, chunk.len());
                        let params: Vec<SqlParam> = chunk
                            .iter()
                            .flatten()
                            .map(|v| {
                                cell_to_param(v, &mut outcome.warnings, &mut non_finite_warned)
                            })
                            .collect();
                        if let Err(err) = sink.execute(&sql, &params).await {
                            let _ = sink.rollback().await;
                            outcome.error = Some(transfer_error(&label, err));
                            let _ = progress
                                .send(TransferProgress::Finished(outcome.clone()))
                                .await;
                            return outcome;
                        }
                        copied += chunk.len() as u64;
                        let _ = progress
                            .send(TransferProgress::Rows {
                                table: label.clone(),
                                copied,
                            })
                            .await;
                    }
                }
                QueryEvent::Error { message } => {
                    let _ = source.queries.cancel(&handle.task_id).await;
                    let _ = sink.rollback().await;
                    outcome.error = Some(transfer_error(&label, message));
                    let _ = progress
                        .send(TransferProgress::Finished(outcome.clone()))
                        .await;
                    return outcome;
                }
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }

        if !buffer.is_empty() {
            if cancel.is_requested() {
                let _ = source.queries.cancel(&handle.task_id).await;
                let _ = sink.rollback().await;
                outcome.cancelled = true;
                let _ = progress
                    .send(TransferProgress::Finished(outcome.clone()))
                    .await;
                return outcome;
            }
            let sql = insert_sql(plan, table, buffer.len());
            let params: Vec<SqlParam> = buffer
                .iter()
                .flatten()
                .map(|v| cell_to_param(v, &mut outcome.warnings, &mut non_finite_warned))
                .collect();
            if let Err(err) = sink.execute(&sql, &params).await {
                let _ = sink.rollback().await;
                outcome.error = Some(transfer_error(&label, err));
                let _ = progress
                    .send(TransferProgress::Finished(outcome.clone()))
                    .await;
                return outcome;
            }
            copied += buffer.len() as u64;
            let _ = progress
                .send(TransferProgress::Rows {
                    table: label.clone(),
                    copied,
                })
                .await;
        }

        for sql in &stmts.post {
            if let Err(err) = sink.execute(sql, &[]).await {
                let _ = sink.rollback().await;
                outcome.error = Some(transfer_error(&label, err));
                let _ = progress
                    .send(TransferProgress::Finished(outcome.clone()))
                    .await;
                return outcome;
            }
        }

        if let Err(err) = sink.commit().await {
            outcome.error = Some(transfer_error(&label, err));
            let _ = progress
                .send(TransferProgress::Finished(outcome.clone()))
                .await;
            return outcome;
        }

        outcome.tables.push((label.clone(), copied));
        let _ = progress
            .send(TransferProgress::TableFinished {
                table: label,
                copied,
            })
            .await;
    }

    let _ = progress
        .send(TransferProgress::Finished(outcome.clone()))
        .await;
    outcome
}
