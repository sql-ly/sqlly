//! The trusted side of every tool call. The host decides, from the mode's
//! inventory, the notebook's trust decision, the connection slots the app
//! handed it and the budget meter, whether a call happens at all; the
//! model's words never widen that. Connection tools go through the app's
//! read-only [`AgentToolHost`] per slot (the existing `ServicesToolHost`:
//! SELECT-only gate plus the database-enforced read-only session), after the
//! host's own SELECT-only check for the slot's engine.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use sqlly_ai::catalog::llm::{ToolCall, ToolResult, ToolSpec};
use sqlly_ai::catalog::{AgentToolHost, MAX_TOOL_RESULT_CHARS, window_tool_text};

use super::budget::{BudgetMeter, Clock, Pricing};
use super::context::{self, NotebookContext, hex, writes};
use super::findings::{QueryEvidence, RunEvidence};
use super::proposals::{self, Refusal};
use super::tools::{self, NotebookTool};
use crate::notebook::cells::ai::{
    AgentMode, Budget, BudgetKind, ObservedOutput, Proposal, ProposalAction, ProposalStatus,
    QueryRecord,
};
use crate::notebook::run_record::sql_hash;
use crate::notebook_security::prompt::{UntrustedSource, fence_untrusted};
use crate::notebook_security::redact::{looks_sensitive_column, mask_secrets};

/// One tool call as the action log keeps it: hashes, never text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLog {
    pub name: String,
    pub arguments_hash: String,
    pub result_hash: String,
    pub is_error: bool,
    /// The budget cap that refused the call, if one did.
    pub refused_by: Option<BudgetKind>,
}

/// What the host collected during a run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostRecords {
    pub queries: Vec<QueryRecord>,
    pub evidence: RunEvidence,
    pub proposals: Vec<Proposal>,
    pub observed: BTreeMap<String, ObservedOutput>,
    pub log: Vec<ToolLog>,
    /// Calls refused by a budget cap.
    pub budget_refusals: Vec<BudgetKind>,
    /// Calls to slots the agent may not use.
    pub connection_refusals: usize,
}

pub struct NotebookToolHost {
    ctx: NotebookContext,
    mode: AgentMode,
    inventory: Vec<&'static str>,
    connections: BTreeMap<String, Arc<dyn AgentToolHost>>,
    meter: BudgetMeter,
    clock: Clock,
    records: Mutex<HostRecords>,
}

fn hash(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// Rows a `ServicesToolHost` pipe table reports, and whether it was cut
/// short: read from its trailer line (`(N rows shown…)`), never from a
/// note after it or a value inside it.
fn rows_reported(text: &str) -> (u64, bool) {
    let trailer = text.lines().rev().find_map(|line| {
        let rest = line.strip_prefix('(')?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let after = &rest[digits.len()..];
        after.starts_with(" rows shown").then(|| {
            (
                digits.parse().unwrap_or(0),
                after.contains("more available"),
            )
        })
    });
    trailer.unwrap_or((0, false))
}

/// For each item of `sql`'s first select list, whether it reads a
/// sensitive-looking column (so `SELECT password AS x` hides `x`). `None`
/// when the list can't be read item by item (no SELECT, or a `*`).
fn sensitive_select_items(sql: &str) -> Option<Vec<bool>> {
    let lower = sql.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let word_at = |i: usize, word: &str| {
        lower[i..].starts_with(word)
            && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            && bytes
                .get(i + word.len())
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
    };
    let (mut depth, mut quote, mut start, mut items) = (0i32, None::<u8>, None, Vec::new());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'\'' | b'"' | b'`' => quote = Some(b),
                b'[' => quote = Some(b']'),
                b'(' => depth += 1,
                b')' => depth -= 1,
                b',' if depth == 0 && start.is_some() => {
                    items.push(&sql[start.unwrap()..i]);
                    start = Some(i + 1);
                }
                _ if depth == 0 && start.is_none() && word_at(i, "select") => {
                    start = Some(i + "select".len());
                }
                _ if depth == 0 && start.is_some() && word_at(i, "from") => break,
                _ => {}
            },
        }
        i += 1;
    }
    let start = start?;
    items.push(&sql[start..i.min(sql.len())]);
    if items.iter().any(|item| item.contains('*')) {
        return None;
    }
    Some(
        items
            .iter()
            .map(|item| {
                item.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|word| !word.is_empty() && looks_sensitive_column(word))
            })
            .collect(),
    )
}

/// Mask sensitive columns and secret-shaped values in a pipe table: by
/// column position, from each column's name and (when `sql` is known) what
/// its select-list item reads.
fn mask_table(text: &str, sql: Option<&str>) -> String {
    let masked = mask_secrets(text).0;
    let mut lines = masked.lines();
    let Some(header) = lines.next() else {
        return masked;
    };
    let names: Vec<&str> = header.split(" | ").collect();
    let reads = sql.and_then(sensitive_select_items);
    let hidden: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(i, name)| {
            looks_sensitive_column(name.trim())
                || reads
                    .as_ref()
                    .is_some_and(|r| r.len() == names.len() && r[*i])
        })
        .map(|(i, _)| i)
        .collect();
    if hidden.is_empty() {
        return masked;
    }
    let trailer = |line: &str| {
        line.strip_prefix('(').is_some_and(|rest| {
            rest.starts_with("note:")
                || rest
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .starts_with(" rows shown")
        })
    };
    let mut out = vec![header.to_string()];
    for line in lines {
        if trailer(line) {
            out.push(line.to_string());
            continue;
        }
        let cells: Vec<String> = line
            .split(" | ")
            .enumerate()
            .map(|(i, v)| {
                if hidden.contains(&i) {
                    "[hidden]".to_string()
                } else {
                    v.to_string()
                }
            })
            .collect();
        out.push(cells.join(" | "));
    }
    out.join("\n")
}

fn budget_text(kind: BudgetKind, budget: &Budget) -> String {
    let cap = match kind {
        BudgetKind::Queries => format!("{} queries", budget.max_queries),
        BudgetKind::Rows => format!("{} rows", budget.max_rows),
        BudgetKind::Runtime => format!("{} ms of query time", budget.max_runtime_ms),
        BudgetKind::Tokens => format!("{} tokens", budget.max_tokens),
        BudgetKind::Turns => format!("{} turns", budget.max_turns),
        BudgetKind::Cost => "the cost cap".to_string(),
    };
    format!(
        "Refused: the run's budget of {cap} is used up. Write your findings now from the evidence you have, without more tool calls."
    )
}

enum Outcome {
    Ok(String),
    Err(String),
    Budget(BudgetKind),
}

impl NotebookToolHost {
    pub fn new(
        ctx: NotebookContext,
        mode: AgentMode,
        budget: &Budget,
        pricing: Pricing,
        connections: BTreeMap<String, Arc<dyn AgentToolHost>>,
        clock: Clock,
    ) -> NotebookToolHost {
        NotebookToolHost {
            inventory: tools::inventory(mode),
            ctx,
            mode,
            connections,
            meter: BudgetMeter::new(budget, pricing),
            clock,
            records: Mutex::new(HostRecords::default()),
        }
    }

    pub fn context(&self) -> &NotebookContext {
        &self.ctx
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        tools::specs(self.mode)
    }

    pub fn inventory(&self) -> &[&'static str] {
        &self.inventory
    }

    pub fn meter(&self) -> &BudgetMeter {
        &self.meter
    }

    fn records(&self) -> std::sync::MutexGuard<'_, HostRecords> {
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> HostRecords {
        self.records().clone()
    }

    /// Mark a cell's output as read by the run (deterministic checks count
    /// as reading it).
    pub fn note_read(&self, id: &str) {
        if let Some(output) = self.ctx.outputs.get(id) {
            let parts = self.ctx.runtime.get(id).and_then(|r| r.parts.clone());
            let mut records = self.records();
            records.evidence.read_outputs.insert(id.to_string());
            records
                .observed
                .insert(id.to_string(), context::observed(output, parts));
        }
    }

    /// The connection a call may use: a declared slot that the notebook's
    /// trust authorizes and the app gave a read-only host for.
    fn slot(&self, requested: Option<&str>) -> Result<(String, Arc<dyn AgentToolHost>), String> {
        let slot = requested
            .map(str::to_string)
            .unwrap_or_else(|| self.ctx.slot_of(None));
        let authorized = self.ctx.connections.get(&slot).is_some_and(|facts| {
            self.ctx
                .trust
                .authorizes(&slot, facts.profile_id.as_deref())
        });
        match self.connections.get(&slot) {
            Some(host) if authorized => Ok((slot, Arc::clone(host))),
            _ => {
                self.records().connection_refusals += 1;
                let allowed: Vec<&str> =
                    self.connections
                        .keys()
                        .filter(|s| {
                            self.ctx.connections.get(*s).is_some_and(|f| {
                                self.ctx.trust.authorizes(s, f.profile_id.as_deref())
                            })
                        })
                        .map(String::as_str)
                        .collect();
                Err(format!(
                    "Refused: connection '{slot}' is not available to the notebook agent. Connections you may use: {}.",
                    if allowed.is_empty() {
                        "none".to_string()
                    } else {
                        allowed.join(", ")
                    }
                ))
            }
        }
    }

    fn propose(
        &self,
        action: ProposalAction,
        rationale: String,
        check: Result<(), Refusal>,
    ) -> Outcome {
        let mut records = self.records();
        let id = format!("p{}", records.proposals.len() + 1);
        let (status, refusal, text) = match &check {
            Ok(()) => (ProposalStatus::Pending, None, None),
            Err(r) => (ProposalStatus::Refused, Some(r.stored()), Some(r.english())),
        };
        records.proposals.push(Proposal {
            id: id.clone(),
            action,
            rationale,
            status,
            refusal,
            refusal_text: text.clone(),
        });
        match text {
            Some(text) => Outcome::Err(text),
            None => Outcome::Ok(format!(
                "Proposal {id} recorded. Nothing has changed: the user will accept or reject it."
            )),
        }
    }

    async fn query(
        &self,
        connection: Option<&str>,
        sql: Option<&str>,
        table: Option<(&str, usize)>,
        limit: usize,
    ) -> Outcome {
        let (slot, host) = match self.slot(connection) {
            Ok(found) => found,
            Err(text) => return Outcome::Err(text),
        };
        if let Some(sql) = sql
            && writes(self.ctx.engine_of(&slot), sql)
        {
            return Outcome::Err(Refusal::WriteStatement { cell: None }.english());
        }
        let limit = match self.meter.reserve_query(limit as u64) {
            Ok(limit) => limit as usize,
            Err(kind) => return Outcome::Budget(kind),
        };
        let started = (self.clock)();
        let result = match (sql, table) {
            (Some(sql), _) => host.run_readonly_query(sql, limit).await,
            (None, Some((table, _))) => {
                host.get_sample_rows(table, limit.min(sqlly_ai::catalog::MAX_SAMPLE_ROWS))
                    .await
            }
            (None, None) => Err("nothing to run".to_string()),
        };
        let elapsed = (self.clock)().saturating_sub(started);
        let text_for_record = sql
            .map(str::to_string)
            .unwrap_or_else(|| format!("sample rows of {}", table.map(|t| t.0).unwrap_or("")));
        let mut records = self.records();
        let id = format!("q{}", records.queries.len() + 1);
        let mut record = QueryRecord {
            id: id.clone(),
            connection: slot.clone(),
            sql: mask_secrets(&text_for_record).0,
            sql_hash: sql_hash(&text_for_record),
            elapsed_ms: elapsed,
            ..QueryRecord::default()
        };
        let outcome = match result {
            Ok(text) => {
                let (rows, cut_short) = rows_reported(&text);
                self.meter.charge_query(rows, elapsed);
                let masked = mask_table(&text, sql);
                record.rows = rows;
                record.result_hash = Some(hash(&masked));
                records.evidence.queries.insert(
                    id.clone(),
                    QueryEvidence {
                        ok: true,
                        text: masked.clone(),
                        result_hash: record.result_hash.clone(),
                        cut_short,
                    },
                );
                Outcome::Ok(format!(
                    "Query {id} on {slot} ({rows} rows{}). Cite it as \"{id}\".\n{}",
                    if cut_short {
                        ", cut at the row limit"
                    } else {
                        ""
                    },
                    fence_untrusted(&masked, UntrustedSource::RowValues)
                ))
            }
            Err(error) => {
                self.meter.charge_query(0, elapsed);
                let error = mask_secrets(&error).0;
                record.error = Some(error.clone());
                records
                    .evidence
                    .queries
                    .insert(id.clone(), QueryEvidence::default());
                Outcome::Err(format!(
                    "Query {id} failed:\n{}",
                    fence_untrusted(&error, UntrustedSource::ResultMetadata)
                ))
            }
        };
        records.queries.push(record);
        outcome
    }

    async fn run(&self, tool: NotebookTool) -> Outcome {
        match tool {
            NotebookTool::ListCells => Outcome::Ok(context::outline(&self.ctx)),
            NotebookTool::ReadCell { cell } => match self.ctx.resolve_cell(&cell) {
                Some(found) => Outcome::Ok(context::read_cell(&self.ctx, &found.id.clone())),
                None => Outcome::Err(format!("There is no cell '{cell}'.")),
            },
            NotebookTool::ReadOutputSummary { cell, sample_rows } => {
                let Some(found) = self.ctx.resolve_cell(&cell) else {
                    return Outcome::Err(format!("There is no cell '{cell}'."));
                };
                let id = found.id.clone();
                let Some(output) = self.ctx.outputs.get(&id) else {
                    return Outcome::Err(format!(
                        "Cell '{cell}' has no result in this session. Propose running it instead of guessing its data."
                    ));
                };
                let wanted = sample_rows.min(output.rows.len()) as u64;
                let granted = match self.meter.reserve_rows(wanted) {
                    Ok(granted) => granted as usize,
                    Err(_) => 0,
                };
                self.note_read(&id);
                let mut text = context::output_summary(&self.ctx, &id, granted).unwrap_or_default();
                if (granted as u64) < wanted {
                    text.push_str("\n(sample rows limited by the row budget)");
                }
                Outcome::Ok(text)
            }
            NotebookTool::ListTables { connection, schema } => {
                match self.slot(connection.as_deref()) {
                    Ok((_, host)) => match host.list_tables(schema.as_deref()).await {
                        Ok(text) => {
                            Outcome::Ok(fence_untrusted(&text, UntrustedSource::ObjectName))
                        }
                        Err(e) => Outcome::Err(fence_untrusted(
                            &mask_secrets(&e).0,
                            UntrustedSource::ResultMetadata,
                        )),
                    },
                    Err(text) => Outcome::Err(text),
                }
            }
            NotebookTool::GetColumns { connection, table } => {
                match self.slot(connection.as_deref()) {
                    Ok((_, host)) => match host.get_columns(&table).await {
                        Ok(text) => {
                            Outcome::Ok(fence_untrusted(&text, UntrustedSource::ObjectName))
                        }
                        Err(e) => Outcome::Err(fence_untrusted(
                            &mask_secrets(&e).0,
                            UntrustedSource::ResultMetadata,
                        )),
                    },
                    Err(text) => Outcome::Err(text),
                }
            }
            NotebookTool::GetSampleRows {
                connection,
                table,
                limit,
            } => {
                self.query(connection.as_deref(), None, Some((&table, limit)), limit)
                    .await
            }
            NotebookTool::RunReadonlyQuery {
                connection,
                sql,
                limit,
            } => {
                let limit = limit.clamp(1, sqlly_ai::catalog::MAX_QUERY_ROWS);
                self.query(connection.as_deref(), Some(&sql), None, limit)
                    .await
            }
            NotebookTool::ProposeCells {
                after,
                cells,
                rationale,
            } => {
                let checked = proposals::normalize_insert(&self.ctx, after.as_deref(), &cells);
                let after = after
                    .as_deref()
                    .and_then(|a| self.ctx.resolve_cell(a))
                    .map(|c| c.id.clone());
                let (cells, check) = match checked {
                    Ok(normalized) => (normalized, Ok(())),
                    Err(refusal) => (cells, Err(refusal)),
                };
                self.propose(
                    ProposalAction::InsertCells { after, cells },
                    rationale,
                    check,
                )
            }
            NotebookTool::ProposeEdit {
                cell,
                source,
                spec,
                rationale,
            } => {
                let id = self
                    .ctx
                    .resolve_cell(&cell)
                    .map(|c| c.id.clone())
                    .unwrap_or(cell);
                let mut check = proposals::check_edit(&self.ctx, &id, source.as_deref());
                let mut spec = spec;
                if check.is_ok()
                    && let (Some(new), Some(target)) = (&spec, self.ctx.doc.cell(&id))
                {
                    match proposals::normalize_spec(target.kind.as_str(), new).and_then(
                        |normalized| {
                            proposals::check_spec_edit(&self.ctx, &id, &normalized)
                                .map(|()| normalized)
                        },
                    ) {
                        Ok(normalized) => spec = Some(normalized),
                        Err(refusal) => check = Err(refusal),
                    }
                }
                let mut diff = match (&source, self.ctx.doc.cell(&id)) {
                    (Some(new), Some(old)) => proposals::line_diff(&old.source, new),
                    _ => vec![],
                };
                if let (Some(new), Some(old)) = (&spec, self.ctx.doc.cell(&id)) {
                    diff.extend(proposals::spec_diff(&old.spec, new));
                }
                self.propose(
                    ProposalAction::EditCell {
                        cell_id: id,
                        source,
                        spec,
                        diff,
                    },
                    rationale,
                    check,
                )
            }
            NotebookTool::ProposeRun { cells, rationale } => {
                let ids: Vec<String> = cells
                    .iter()
                    .map(|c| {
                        self.ctx
                            .resolve_cell(c)
                            .map(|f| f.id.clone())
                            .unwrap_or_else(|| c.clone())
                    })
                    .collect();
                let check = proposals::check_run(&self.ctx, &ids);
                self.propose(ProposalAction::RunCells { cells: ids }, rationale, check)
            }
        }
    }

    /// Run one call: inventory, parse, policy, budget, then the tool.
    pub async fn dispatch(&self, call: &ToolCall) -> ToolResult {
        let outcome = if !self.inventory.contains(&call.name.as_str()) {
            if tools::ALL_TOOLS.contains(&call.name.as_str()) {
                Outcome::Err(format!(
                    "Refused: {} is not available in {} mode.",
                    call.name,
                    self.mode.as_str()
                ))
            } else {
                Outcome::Err(tools::ParseError::UnknownTool(call.name.clone()).to_model_text())
            }
        } else {
            match tools::parse(call) {
                Ok(tool) => self.run(tool).await,
                Err(error) => Outcome::Err(error.to_model_text()),
            }
        };
        let (content, is_error, refused_by) = match outcome {
            Outcome::Ok(text) => (window_tool_text(&text, MAX_TOOL_RESULT_CHARS), false, None),
            Outcome::Err(text) => (window_tool_text(&text, MAX_TOOL_RESULT_CHARS), true, None),
            Outcome::Budget(kind) => (budget_text(kind, self.meter.budget()), true, Some(kind)),
        };
        let mut records = self.records();
        if let Some(kind) = refused_by {
            records.budget_refusals.push(kind);
        }
        records.log.push(ToolLog {
            name: call.name.clone(),
            arguments_hash: hash(&call.arguments.to_string()),
            result_hash: hash(&content),
            is_error,
            refused_by,
        });
        ToolResult {
            call_id: call.id.clone(),
            content,
            is_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_counts_are_read_from_the_pipe_table_trailer() {
        assert_eq!(rows_reported("a | b\n1 | 2\n(1 rows shown)"), (1, false));
        assert_eq!(
            rows_reported("a\n(100 rows shown, more available)"),
            (100, true)
        );
        assert_eq!(rows_reported("(no columns)"), (0, false));
    }

    #[test]
    fn the_row_count_comes_from_the_trailer_not_a_note_or_a_value() {
        assert_eq!(
            rows_reported(
                "a\n(x) (7 rows shown)\n(100 rows shown, more available)\n(note: only the first result set is shown)"
            ),
            (100, true)
        );
    }

    #[test]
    fn values_cannot_shift_columns_and_aliases_stay_hidden() {
        let cell = crate::agent_tools::pipe_cell;
        let table = format!(
            "{} | {}\n{} | {}\n(1 rows shown)",
            cell("note"),
            cell("password"),
            cell("a | b\nc"),
            cell("hunter2")
        );
        let masked = mask_table(&table, None);
        assert!(!masked.contains("hunter2"), "{masked}");
        assert!(
            masked.lines().nth(1).unwrap().ends_with("[hidden]"),
            "{masked}"
        );
        let parenthesized = mask_table("password\n(hunter2)\n(1 rows shown)", None);
        assert!(!parenthesized.contains("hunter2"), "{parenthesized}");
        let aliased = mask_table(
            "id | x\n1 | hunter2\n(1 rows shown)",
            Some("SELECT id, lower(u.password) AS x FROM users u"),
        );
        assert!(!aliased.contains("hunter2"), "{aliased}");
        assert!(aliased.contains("1 | [hidden]"));
        assert_eq!(
            sensitive_select_items("SELECT id, 'a,b' AS t, f(ssn, 2) s FROM p"),
            Some(vec![false, false, true])
        );
        assert_eq!(sensitive_select_items("SELECT * FROM p"), None);
    }

    #[test]
    fn sensitive_columns_are_masked_in_query_results() {
        let masked = mask_table(
            "id | password | note\n1 | hunter2 | ok\n(1 rows shown)",
            None,
        );
        assert!(!masked.contains("hunter2"));
        assert!(masked.contains("1 | [hidden] | ok"));
        assert!(masked.ends_with("(1 rows shown)"));
    }
}
