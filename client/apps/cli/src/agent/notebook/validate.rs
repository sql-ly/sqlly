//! `sqlly notebook validate`: everything that would stop or endanger a run,
//! found without running anything. Suitable for CI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_services::notebook::expand::substitute_relations;
use sqlly_services::notebook::graph::Node;
use sqlly_services::notebook::params::{ParamValue, SystemClock, resolve_all};
use sqlly_services::notebook::references::validate_names;
use sqlly_services::notebook::{
    CellKind, ExecClass, ExpandOptions, Expansion, Graph, NotebookDoc, ResolvedParams, expand_sql,
};

use super::remap::{self, LOCAL_SLOT, SlotTarget, Slots, UnmappedReason};
use super::{NotebookFormat, params, source, specs};
use crate::agent::connections::StoreFile;
use crate::agent::{CliError, ExitCode};

#[derive(Debug, Clone, clap::Args)]
pub struct ValidateArgs {
    pub file: PathBuf,
    /// Map a connection slot for the checks (`slot=<connection>`).
    #[arg(long = "connection", value_name = "SLOT=CONNECTION")]
    pub connections: Vec<String>,
    /// Parameter value to validate with, `name=value`.
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,
    #[arg(long)]
    pub param_set: Option<PathBuf>,
    /// Treat slots not mapped on this machine as errors (default: warnings,
    /// so CI without saved connections can still validate).
    #[arg(long)]
    pub require_connections: bool,
    /// Treat warnings as errors.
    #[arg(long)]
    pub strict: bool,
    #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
    pub format: NotebookFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Diagnostic {
    fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            cell_id: None,
            cell: None,
            message: message.into(),
            hint: None,
        }
    }

    fn at(mut self, doc: &NotebookDoc, cell_id: &str) -> Self {
        self.cell = doc.cell(cell_id).map(|c| c.relation_name().to_string());
        self.cell_id = Some(cell_id.to_string());
        self
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

pub struct Inputs<'a> {
    pub doc: &'a NotebookDoc,
    pub migrated_from: Option<u32>,
    pub slots: &'a Slots,
    pub resolved: &'a ResolvedParams,
    pub supplied: &'a BTreeMap<String, ParamValue>,
    pub require_connections: bool,
}

/// Every problem in a notebook, errors first, then in document order.
pub fn diagnose(inputs: &Inputs<'_>) -> Vec<Diagnostic> {
    let doc = inputs.doc;
    let slots = inputs.slots;
    let dialects = slots.dialects();
    let graph = Graph::build(doc, &dialects);
    let mut out: Vec<Diagnostic> = Vec::new();

    if let Some(version) = inputs.migrated_from {
        out.push(
            Diagnostic::new(
                Severity::Info,
                "legacy_format",
                format!("this is a version {version} notebook; it was read as the current format"),
            )
            .hint("save it as .sqllynb to keep stable ids"),
        );
    }
    for (cell_id, error) in validate_names(doc) {
        out.push(
            Diagnostic::new(Severity::Error, "invalid_name", error.message().fallback())
                .at(doc, &cell_id),
        );
    }
    for cycle in graph.cycles() {
        let first = cycle
            .path
            .iter()
            .find_map(Node::as_cell)
            .unwrap_or_default();
        out.push(
            Diagnostic::new(Severity::Error, "cycle", cycle.message().fallback())
                .at(doc, first)
                .hint(cycle.suggestion().fallback()),
        );
    }
    for unresolved in graph.unresolved() {
        let owner = unresolved.owner();
        let mut d = Diagnostic::new(
            Severity::Error,
            "unresolved_reference",
            unresolved.message().fallback(),
        );
        if let Some(cell) = owner.as_cell() {
            d = d.at(doc, cell);
        }
        out.push(d);
    }

    let mut reported_slots: BTreeSet<String> = BTreeSet::new();
    let cyclic: BTreeSet<String> = graph
        .cyclic()
        .iter()
        .filter_map(|n| n.as_cell().map(str::to_string))
        .collect();
    for cell in &doc.cells {
        match &cell.kind {
            CellKind::Unknown(kind) => out.push(
                Diagnostic::new(
                    Severity::Warning,
                    "unknown_kind",
                    format!("cell kind '{kind}' comes from a newer SQLLY; it is kept but not run"),
                )
                .at(doc, &cell.id),
            ),
            CellKind::Python => out.push(
                Diagnostic::new(
                    Severity::Warning,
                    "unsupported_kind",
                    "Python cells are kept but not run in this version",
                )
                .at(doc, &cell.id),
            ),
            CellKind::Assertion => {
                if let Err(e) = specs::assertion_spec(cell) {
                    out.push(
                        Diagnostic::new(
                            Severity::Error,
                            "invalid_spec",
                            format!("assertion cell {}", e.0),
                        )
                        .at(doc, &cell.id),
                    );
                }
            }
            CellKind::Kpi => {
                if let Err(e) = specs::kpi_spec(cell) {
                    out.push(
                        Diagnostic::new(
                            Severity::Error,
                            "invalid_spec",
                            format!("KPI cell {}", e.0),
                        )
                        .at(doc, &cell.id),
                    );
                }
            }
            _ => {}
        }
        if cell.exec_class() != ExecClass::Remote {
            continue;
        }
        let key = Slots::key_for(doc, cell);
        let write = !cell.execution_policy.read_only;
        match (slots.get(&key), slots.unmapped.get(&key)) {
            (None, Some(unmapped)) => {
                if reported_slots.insert(key.clone()) {
                    let (severity, code) = match unmapped.reason {
                        UnmappedReason::NotDeclared => (Severity::Error, "undeclared_connection"),
                        _ if inputs.require_connections => (Severity::Error, "unmapped_connection"),
                        _ => (Severity::Warning, "unmapped_connection"),
                    };
                    out.push(
                        Diagnostic::new(severity, code, unmapped.message())
                            .at(doc, &cell.id)
                            .hint(unmapped.hint()),
                    );
                }
            }
            (Some(SlotTarget::Local), _) if write => out.push(
                Diagnostic::new(
                    Severity::Error,
                    "local_write",
                    "the cell changes data, but the notebook's local data is read-only",
                )
                .at(doc, &cell.id),
            ),
            (
                Some(SlotTarget::Remote {
                    profile,
                    production,
                }),
                _,
            ) => {
                if write && *production {
                    out.push(
                        Diagnostic::new(
                            Severity::Error,
                            "write_on_production",
                            format!(
                                "the cell changes data on the production connection '{}'; unattended runs never write to production",
                                profile.name
                            ),
                        )
                        .at(doc, &cell.id),
                    );
                } else if *production && reported_slots.insert(format!("prod:{key}")) {
                    out.push(
                        Diagnostic::new(
                            Severity::Warning,
                            "production_connection",
                            format!("cells run on the production connection '{}'", profile.name),
                        )
                        .at(doc, &cell.id)
                        .hint("runs need --allow-production"),
                    );
                }
            }
            _ => {}
        }
        if cell.kind != CellKind::Sql || cyclic.contains(&cell.id) {
            continue;
        }
        if write && !matches!(slots.get(&key), Some(SlotTarget::Local)) {
            out.push(
                Diagnostic::new(
                    Severity::Warning,
                    "write_cell",
                    "the cell is allowed to change data",
                )
                .at(doc, &cell.id)
                .hint("runs need --allow-writes; mark the cell read-only if it only reads"),
            );
        }
        let db = dialects.for_cell(doc, cell);
        if !write
            && let Ok(text) = substitute_relations(doc, &cell.id, &dialects, &|c| {
                db.quote_identifier(c.relation_name())
            })
            && !text.trim().is_empty()
        {
            let class = classify_select_only_for(db, &text);
            if !class.select_only {
                out.push(
                    Diagnostic::new(
                        Severity::Error,
                        "read_only_violation",
                        format!(
                            "the cell is read-only but its SQL is not: {}",
                            class.reason.unwrap_or_else(|| "it changes data".into())
                        ),
                    )
                    .at(doc, &cell.id)
                    .hint("remove the statement, or allow the cell to change data"),
                );
            }
        }
        let materialized = BTreeSet::new();
        match expand_sql(
            doc,
            &cell.id,
            ExpandOptions {
                dialects: &dialects,
                materialized: &materialized,
            },
        ) {
            Err(e) => out.push(
                Diagnostic::new(Severity::Error, "expansion", e.message().fallback())
                    .at(doc, &cell.id),
            ),
            Ok(Expansion::NeedsLocalMaterialization { cells }) if key != LOCAL_SLOT => {
                let names: Vec<String> = cells
                    .iter()
                    .map(|id| {
                        doc.cell(id)
                            .map_or(id.clone(), |c| c.relation_name().to_string())
                    })
                    .collect();
                out.push(
                    Diagnostic::new(
                        Severity::Error,
                        "needs_local",
                        format!(
                            "the cell reads {} from another connection or a non-SQL cell",
                            names.join(", ")
                        ),
                    )
                    .at(doc, &cell.id)
                    .hint(format!(
                        "set the cell's connection to `{LOCAL_SLOT}` to join them locally"
                    )),
                );
            }
            Ok(_) => {}
        }
    }

    let used: BTreeSet<String> = doc
        .cells
        .iter()
        .flat_map(|c| {
            graph
                .params_of(&c.id)
                .into_iter()
                .map(str::to_ascii_lowercase)
        })
        .collect();
    for (name, error) in &inputs.resolved.errors {
        let supplied = inputs.supplied.keys().any(|k| k.eq_ignore_ascii_case(name));
        let severity = if supplied || used.contains(&name.to_ascii_lowercase()) {
            Severity::Error
        } else {
            Severity::Warning
        };
        out.push(
            Diagnostic::new(
                severity,
                "parameter",
                format!("parameter '{name}': {}", error.message().fallback()),
            )
            .hint("pass --param or --param-set, or give it a default"),
        );
    }

    let order: BTreeMap<&str, usize> = doc
        .cells
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    out.sort_by_key(|d| {
        (
            d.severity,
            d.cell_id
                .as_deref()
                .and_then(|id| order.get(id).copied())
                .unwrap_or(usize::MAX),
        )
    });
    out
}

/// Lane 9's file scan as diagnostics: blocking findings are errors.
pub fn scan_diagnostics(
    scan: &sqlly_services::notebook_security::scan::ScanReport,
) -> Vec<Diagnostic> {
    use sqlly_services::notebook_security::scan::Severity as ScanSeverity;
    scan.findings
        .iter()
        .map(|f| {
            let severity = match f.severity {
                ScanSeverity::Reject => Severity::Error,
                _ => Severity::Warning,
            };
            let mut d = Diagnostic::new(severity, f.finding_key(), f.message().fallback());
            if !f.location.is_empty() {
                d.hint = Some(format!("at {}", f.location));
            }
            d
        })
        .collect()
}

pub fn render_text(file: &str, diagnostics: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diagnostics {
        let severity = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        let at = d
            .cell
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        out.push_str(&format!("{severity}: {}{at}: {}\n", d.code, d.message));
        if let Some(hint) = &d.hint {
            out.push_str(&format!("  hint: {hint}\n"));
        }
    }
    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    if errors == 0 && warnings == 0 {
        out.push_str(&format!("{file}: ok"));
    } else {
        out.push_str(&format!("{file}: {errors} error(s), {warnings} warning(s)"));
    }
    out
}

pub fn command(args: &ValidateArgs, store: &StoreFile, json: bool) -> ExitCode {
    let file = args.file.display().to_string();
    let result = (|| -> Result<(Vec<Diagnostic>, String), CliError> {
        let (loaded, scan) = match source::load_scanned(&args.file)? {
            source::Scanned::Loaded(loaded, scan) => (*loaded, scan),
            source::Scanned::Rejected(scan) => return Ok((scan_diagnostics(&scan), String::new())),
        };
        let doc = loaded.doc;
        let param_set = match &args.param_set {
            Some(path) => params::read_param_set(path)?,
            None => BTreeMap::new(),
        };
        let supplied = params::collect(&doc, &param_set, &args.params)?;
        let resolved = resolve_all(&doc.parameters, &supplied, &SystemClock);
        let mappings = remap::parse_mappings(&doc, &args.connections)?;
        let slots = remap::resolve(&doc, &mappings, store)?;
        let mut found = scan_diagnostics(&scan);
        found.extend(diagnose(&Inputs {
            doc: &doc,
            migrated_from: loaded.migrated_from,
            slots: &slots,
            resolved: &resolved,
            supplied: &supplied,
            require_connections: args.require_connections,
        }));
        found.sort_by_key(|d| d.severity);
        Ok((found, doc.notebook_id))
    })();
    let (diagnostics, notebook_id) = match result {
        Ok(found) => found,
        Err(err) => {
            // A file that does not parse is a validation error, not a crash:
            // keep the machine-readable shape for CI.
            if err.exit == ExitCode::Usage && (json || args.format == NotebookFormat::Json) {
                let d = Diagnostic::new(Severity::Error, "parse", err.body.message.clone());
                (vec![d], String::new())
            } else {
                return crate::agent::emit(json, None, Err(err));
            }
        }
    };
    let errors = diagnostics
        .iter()
        .filter(|d| {
            d.severity == Severity::Error || (args.strict && d.severity == Severity::Warning)
        })
        .count();
    let exit = if errors > 0 {
        ExitCode::Error
    } else {
        ExitCode::Ok
    };
    let data = serde_json::json!({
        "file": file,
        "notebook_id": notebook_id,
        "valid": errors == 0,
        "errors": diagnostics.iter().filter(|d| d.severity == Severity::Error).count(),
        "warnings": diagnostics.iter().filter(|d| d.severity == Severity::Warning).count(),
        "diagnostics": diagnostics,
    });
    super::finish(
        json,
        args.format,
        render_text(&file, &diagnostics),
        data,
        exit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlly_services::notebook::params::{ParamType, ParameterDef};
    use sqlly_services::notebook::{Cell, ExecutionPolicy};

    fn sql(id: &str, name: &str, source: &str) -> Cell {
        let mut cell = Cell::new(id, CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some(name.into());
        cell.source = source.into();
        cell
    }

    fn run(doc: &NotebookDoc) -> Vec<Diagnostic> {
        let slots = remap::resolve(
            doc,
            &[],
            &StoreFile {
                version: 1,
                profiles: vec![],
                environments: vec![],
            },
        )
        .unwrap();
        let supplied = BTreeMap::new();
        let resolved = resolve_all(&doc.parameters, &supplied, &SystemClock);
        diagnose(&Inputs {
            doc,
            migrated_from: None,
            slots: &slots,
            resolved: &resolved,
            supplied: &supplied,
            require_connections: false,
        })
    }

    fn codes(found: &[Diagnostic]) -> Vec<&'static str> {
        found.iter().map(|d| d.code).collect()
    }

    #[test]
    fn cycles_report_the_full_path_and_a_fix() {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.cells.push(sql("c_a", "a", "SELECT * FROM {{b}}"));
        doc.cells.push(sql("c_b", "b", "SELECT * FROM {{c}}"));
        doc.cells.push(sql("c_c", "c", "SELECT * FROM {{a}}"));
        let found = run(&doc);
        let cycle = found.iter().find(|d| d.code == "cycle").unwrap();
        assert_eq!(cycle.severity, Severity::Error);
        for name in ["a", "b", "c"] {
            assert!(cycle.message.contains(name), "{}", cycle.message);
        }
        assert!(
            cycle
                .hint
                .as_deref()
                .unwrap()
                .starts_with("Remove the reference")
        );
    }

    #[test]
    fn broken_references_kinds_and_specs() {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.cells.push(sql("c_a", "a", "SELECT * FROM {{nope}}"));
        let mut future = Cell::new(
            "c_f",
            CellKind::parse("hologram"),
            ExecutionPolicy::default(),
        );
        future.source = String::new();
        doc.cells.push(future);
        let mut check = Cell::new("c_k", CellKind::Assertion, ExecutionPolicy::default());
        check.spec = json!({"input": "c_a", "kind": "no_such"});
        doc.cells.push(check);
        let found = codes(&run(&doc));
        assert!(found.contains(&"unresolved_reference"), "{found:?}");
        assert!(found.contains(&"unknown_kind"), "{found:?}");
        assert!(found.contains(&"invalid_spec"), "{found:?}");
    }

    #[test]
    fn connection_and_policy_problems() {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.connection_refs =
            vec![serde_json::from_value(json!({"id": "primary", "label": "Sales"})).unwrap()];
        let mut write = sql("c_w", "cleanup", "DELETE FROM t");
        write.execution_policy.read_only = false;
        doc.cells.push(write);
        doc.cells.push(sql("c_r", "sneaky", "DELETE FROM t"));
        let mut ghost = sql("c_g", "ghosted", "SELECT 1");
        ghost.connection_ref = Some("ghost".into());
        doc.cells.push(ghost);
        let found = run(&doc);
        let by_code = |code: &str| found.iter().find(|d| d.code == code).cloned();
        assert_eq!(
            by_code("unmapped_connection").unwrap().severity,
            Severity::Warning
        );
        assert_eq!(
            by_code("undeclared_connection").unwrap().severity,
            Severity::Error
        );
        assert_eq!(
            by_code("write_cell").unwrap().cell.as_deref(),
            Some("cleanup")
        );
        assert_eq!(
            by_code("read_only_violation").unwrap().cell.as_deref(),
            Some("sneaky")
        );
        // Errors sort first.
        assert_eq!(found[0].severity, Severity::Error);
    }

    #[test]
    fn missing_parameters_used_by_cells_are_errors() {
        let mut doc = NotebookDoc::empty("nb", "T");
        let mut day = ParameterDef::new("day", ParamType::Date);
        day.validation.required = true;
        doc.parameters.push(day);
        doc.parameters
            .push(ParameterDef::new("unused", ParamType::Date));
        doc.cells
            .push(sql("c_a", "a", "SELECT * FROM t WHERE d = :day"));
        let found = run(&doc);
        let params: Vec<&Diagnostic> = found.iter().filter(|d| d.code == "parameter").collect();
        assert_eq!(params.len(), 2, "{found:?}");
        assert_eq!(params[0].severity, Severity::Error);
        assert!(params[0].message.contains("day"));
        assert_eq!(params[1].severity, Severity::Warning);
    }

    #[test]
    fn cross_connection_reads_need_the_local_slot() {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.connection_refs = vec![
            serde_json::from_value(json!({"id": "a", "label": "A"})).unwrap(),
            serde_json::from_value(json!({"id": "b", "label": "B"})).unwrap(),
        ];
        let mut left = sql("c_1", "left_side", "SELECT 1 AS id");
        left.connection_ref = Some("a".into());
        let mut right = sql("c_2", "joined", "SELECT * FROM {{left_side}}");
        right.connection_ref = Some("b".into());
        let mut local = sql("c_3", "local_join", "SELECT * FROM {{left_side}}");
        local.connection_ref = Some(LOCAL_SLOT.into());
        doc.cells.extend([left, right, local]);
        let found = run(&doc);
        let needs: Vec<&Diagnostic> = found.iter().filter(|d| d.code == "needs_local").collect();
        assert_eq!(needs.len(), 1, "{found:?}");
        assert_eq!(needs[0].cell.as_deref(), Some("joined"));
    }

    #[test]
    fn blocking_scan_findings_are_errors() {
        let scan = sqlly_services::notebook_security::scan::scan_notebook_file(
            r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","connection_refs":[{"id":"a","label":"x","password":"hunter2"}],"cells":[]}"#,
        );
        let found = scan_diagnostics(&scan);
        assert!(
            found.iter().any(|d| d.severity == Severity::Error),
            "{found:?}"
        );
        assert!(found.iter().all(|d| !d.message.contains("hunter2")));
    }

    #[test]
    fn clean_notebooks_render_ok() {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.cells.push(sql("c_a", "a", "SELECT 1"));
        let found = run(&doc);
        // The default slot is unmapped here: a warning only.
        assert!(
            found.iter().all(|d| d.severity != Severity::Error),
            "{found:?}"
        );
        assert!(render_text("x.sqllynb", &[]).ends_with("x.sqllynb: ok"));
    }

    #[test]
    fn database_dialect_falls_back_to_duckdb_without_connections() {
        let slots = Slots::default();
        assert_eq!(
            slots.dialects().default,
            sqlly_lang::sqlmodel::DatabaseType::DuckDb
        );
    }
}
