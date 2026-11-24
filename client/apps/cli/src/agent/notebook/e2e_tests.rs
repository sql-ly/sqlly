//! `notebook run` end to end against two in-process SQLite databases and
//! the notebook's local DuckDB workspace, all inside a temp directory.

use std::path::{Path, PathBuf};

use serde_json::json;
use sqlly_core::{
    ConnectionEnvironment, ConnectionProfile, DatabaseType, SecretProvider, SecretRef,
};
use sqlly_services::notebook::CellStatus;
use sqlly_services::notebook::fingerprint::ChangeReason;
use sqlly_services::notebook::run_record::RunTrigger;

use super::compare::{self, DataVerdict};
use super::records::RunsDir;
use super::runner::{self, FailOn, RunArgs, RunEnv};
use super::select::RunModeArg;
use crate::agent::ExitCode;
use crate::agent::connections::StoreFile;

fn sqlite(name: &str, path: &Path) -> ConnectionProfile {
    let mut p = ConnectionProfile::sql_login(
        name,
        path.to_string_lossy(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    p.name = name.into();
    p.database_type = DatabaseType::Sqlite;
    p.environment = ConnectionEnvironment::Dev;
    p
}

fn trusted(path: PathBuf) -> RunArgs {
    let mut args = RunArgs::new(path);
    args.trust = true;
    args
}

struct Fixture {
    _dir: tempfile::TempDir,
    notebook: PathBuf,
    env: RunEnv,
}

impl Fixture {
    /// Where the runner keeps this notebook's runs: the data scope of the
    /// one-run grant (derived from the file's path), never the bare id.
    fn key(&self) -> super::records::NotebookKey {
        let scope = std::fs::read_dir(self.env.runs.runs_root())
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| e.path().join("nb_e2e.runs.jsonl").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .unwrap_or_else(|| "nb_e2e".into());
        super::records::NotebookKey {
            scope,
            notebook_id: "nb_e2e".into(),
        }
    }
}

fn fixture(extra_cells: Vec<serde_json::Value>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let sales_db = dir.path().join("sales.db");
    let crm_db = dir.path().join("crm.db");
    rusqlite::Connection::open(&sales_db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE orders(id INTEGER, region TEXT, amount REAL);
             INSERT INTO orders VALUES (1,'EU',12.5),(2,'EU',30),(3,'US',8),(4,'EU',140),(5,'US',20);",
        )
        .unwrap();
    rusqlite::Connection::open(&crm_db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE regions(code TEXT, manager TEXT);
             INSERT INTO regions VALUES ('EU','Ana'),('US','Bo');",
        )
        .unwrap();
    let sales = sqlite("Sales", &sales_db);
    let crm = sqlite("CRM", &crm_db);
    let mut cells = vec![
        json!({"id": "c_intro", "kind": "markdown", "source": "# Orders by region"}),
        json!({"id": "c_orders", "name": "region_orders", "kind": "sql", "connection_ref": "sales",
               "source": "SELECT id, region, amount FROM orders WHERE region = :region ORDER BY id"}),
        json!({"id": "c_big", "name": "big_orders", "kind": "sql", "connection_ref": "sales",
               "source": "SELECT * FROM {{region_orders}} WHERE amount > 20"}),
        json!({"id": "c_regions", "name": "regions", "kind": "sql", "connection_ref": "crm",
               "source": "SELECT code, manager FROM regions"}),
        json!({"id": "c_joined", "name": "by_manager", "kind": "sql", "connection_ref": "local",
               "source": "SELECT r.manager, count(*) AS n, sum(o.amount) AS total FROM {{region_orders}} o JOIN {{regions}} r ON r.code = o.region GROUP BY r.manager"}),
        json!({"id": "c_checks", "name": "checks", "kind": "assertion", "spec": {"input": "region_orders", "assertions": [
            {"kind": "row_count", "min": 1},
            {"kind": "unique", "columns": ["id"]},
            {"kind": "not_null", "column": "amount"}
        ]}}),
        json!({"id": "c_kpi", "name": "revenue", "kind": "kpi", "spec": {"input": "by_manager",
               "value": {"kind": "aggregate", "column": "total", "aggregate": "sum"},
               "comparison": {"kind": "target", "value": 100.0},
               "thresholds": {"basis": "value", "warn": 50.0, "critical": 10.0}}}),
        json!({"id": "c_chart", "name": "chart", "kind": "chart", "spec": {"input": "by_manager", "kind": "bar"}}),
    ];
    cells.extend(extra_cells);
    let doc = json!({
        "format": "sqlly.notebook",
        "format_version": 2,
        "notebook_id": "nb_e2e",
        "title": "Orders",
        "connection_refs": [
            {"id": "sales", "label": "Sales", "profile_id": sales.id.0.to_string()},
            {"id": "crm", "label": "CRM", "profile_id": crm.id.0.to_string()}
        ],
        "parameters": [{"id": "region", "name": "region", "type": "string", "default": "EU"}],
        "cells": cells,
    });
    let notebook = dir.path().join("orders.sqllynb");
    std::fs::write(&notebook, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let env = RunEnv {
        store: StoreFile {
            version: 1,
            profiles: vec![sales, crm],
            environments: vec![],
        },
        runs: RunsDir::new(dir.path().join("data")),
        trigger: RunTrigger::Cli,
    };
    Fixture {
        _dir: dir,
        notebook,
        env,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_record_reuse_and_compare() {
    let fx = fixture(Vec::new());
    let args = trusted(fx.notebook.clone());

    let first = runner::run(&args, &fx.env).await.expect("first run");
    let s = &first.summary;
    assert!(
        s.failed.is_empty() && s.blocked.is_empty(),
        "{s:?} {:?}",
        first.cells
    );
    assert_eq!(s.executed, 6, "{:?}", first.cells);
    assert_eq!(s.skipped, vec!["chart".to_string()]);
    assert_eq!(s.assertions.passed, 3);
    assert_eq!(runner::exit_for(s, FailOn::Assertion), ExitCode::Ok);
    let rows = |report: &runner::RunReport, cell: &str| {
        report
            .cells
            .iter()
            .find(|c| c.cell == cell)
            .and_then(|c| c.rows)
    };
    assert_eq!(rows(&first, "region_orders"), Some(3));
    assert_eq!(rows(&first, "big_orders"), Some(2));
    assert_eq!(rows(&first, "by_manager"), Some(1));
    // 12.5 + 30 + 140 for EU, past the 100 target: good.
    assert_eq!(s.kpis[0].value, Some(182.5));

    let runs = fx.env.runs.list(&fx.key()).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        fx.key().scope.starts_with("p_"),
        "runs follow the trust record, not the declared id"
    );
    let record = &runs[0];
    let checks = record
        .cells
        .iter()
        .find(|c| c.cell_id == "c_checks")
        .unwrap();
    assert_eq!(checks.status, CellStatus::Fresh);
    let orders = record
        .cells
        .iter()
        .find(|c| c.cell_id == "c_orders")
        .unwrap();
    assert_eq!(orders.params[0].value, Some(json!("EU")));
    assert!(
        fx.env
            .runs
            .read_outputs(&fx.key().scope, &record.run_id)
            .is_some()
    );
    // Lineage: the local join names both source connections.
    let outputs = fx
        .env
        .runs
        .read_outputs(&fx.key().scope, &record.run_id)
        .unwrap();
    let sources: Vec<&str> = outputs["c_joined"]
        .provenance
        .sources
        .iter()
        .map(|p| p.connection_label.as_str())
        .collect();
    assert!(
        sources.contains(&"Sales") && sources.contains(&"CRM"),
        "{sources:?}"
    );

    // Stale mode: nothing changed, so everything is reused.
    let mut stale = args.clone();
    stale.mode = RunModeArg::Stale;
    let second = runner::run(&stale, &fx.env).await.expect("stale run");
    assert_eq!(second.summary.executed, 0, "{:?}", second.cells);
    assert_eq!(second.summary.reused, 6);

    // A different parameter re-runs what uses it, and the comparison says why.
    let mut us = args.clone();
    us.params = vec!["region=US".into()];
    let third = runner::run(&us, &fx.env).await.expect("US run");
    let cmp = compare::compare(
        first.record.as_ref().unwrap(),
        third.record.as_ref().unwrap(),
    );
    let orders = cmp.cells.iter().find(|c| c.cell_id == "c_orders").unwrap();
    assert_eq!(orders.reasons, vec![ChangeReason::Parameters]);
    let joined = cmp.cells.iter().find(|c| c.cell_id == "c_joined").unwrap();
    assert_eq!(joined.reasons, vec![ChangeReason::Upstream]);
    assert!(!cmp.cells.iter().any(|c| c.cell_id == "c_regions"));

    // Same query, changed data: the comparison says the data moved.
    let sales_db = fx.notebook.with_file_name("sales.db");
    rusqlite::Connection::open(&sales_db)
        .unwrap()
        .execute("INSERT INTO orders VALUES (6,'EU',1)", [])
        .unwrap();
    let fourth = runner::run(&args, &fx.env).await.expect("after insert");
    let cmp = compare::compare(
        first.record.as_ref().unwrap(),
        fourth.record.as_ref().unwrap(),
    );
    let orders = cmp.cells.iter().find(|c| c.cell_id == "c_orders").unwrap();
    assert!(!orders.fingerprint_changed);
    assert_eq!(orders.data, Some(DataVerdict::Changed));
    assert!(!cmp.cells.iter().any(|c| c.cell_id == "c_regions"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_checks_writes_and_cross_connection_reads() {
    let fx = fixture(vec![
        json!({"id": "c_bad", "name": "bounds", "kind": "assertion",
        "spec": {"input": "region_orders", "kind": "value_range", "column": "amount", "max": 100.0}}),
    ]);
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert_eq!(report.summary.assertions.failed, 1);
    assert_eq!(
        report.summary.assertion_cells_failed,
        vec!["bounds".to_string()]
    );
    assert_eq!(
        runner::exit_for(&report.summary, FailOn::Assertion),
        ExitCode::AssertionFailed
    );
    assert_eq!(
        runner::exit_for(&report.summary, FailOn::Error),
        ExitCode::Ok
    );
    let outcomes = &report.details.assertions["c_bad"];
    assert_eq!(outcomes[0]["evidence"]["failing_count"], 1);

    // A write cell is refused before anything runs.
    let fx = fixture(vec![
        json!({"id": "c_w", "name": "cleanup", "kind": "sql", "connection_ref": "sales",
        "execution_policy": {"read_only": false}, "source": "DELETE FROM orders"}),
    ]);
    let err = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap_err();
    assert_eq!(err.exit, ExitCode::PolicyRefused);
    assert!(fx.env.runs.list(&fx.key()).unwrap().is_empty());

    // Reading another connection's cell without the local slot fails clearly
    // and blocks what depends on it.
    let fx = fixture(vec![
        json!({"id": "c_x", "name": "cross", "kind": "sql", "connection_ref": "crm",
               "source": "SELECT * FROM {{region_orders}}"}),
        json!({"id": "c_y", "name": "after_cross", "kind": "assertion",
               "spec": {"input": "cross", "kind": "row_count", "min": 1}}),
    ]);
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert_eq!(report.summary.failed, vec!["cross".to_string()]);
    assert_eq!(report.summary.blocked, vec!["after_cross".to_string()]);
    let note = report
        .cells
        .iter()
        .find(|c| c.cell == "cross")
        .unwrap()
        .note
        .clone()
        .unwrap();
    assert!(note.contains("local"), "{note}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_runs_and_selections_touch_nothing_extra() {
    let fx = fixture(Vec::new());
    let mut args = trusted(fx.notebook.clone());
    args.dry_run = true;
    let plan = runner::run(&args, &fx.env).await.unwrap();
    assert!(plan.dry_run && plan.record.is_none());
    assert!(fx.env.runs.list(&fx.key()).unwrap().is_empty());
    let mut one = trusted(fx.notebook.clone());
    one.cells = Some("big_orders".into());
    let report = runner::run(&one, &fx.env).await.unwrap();
    let ran: Vec<&str> = report.cells.iter().map(|c| c.cell.as_str()).collect();
    assert_eq!(ran, vec!["region_orders", "big_orders"]);
    // Unknown parameters and unmapped slots are refused up front.
    let mut bad = trusted(fx.notebook.clone());
    bad.params = vec!["nope=1".into()];
    assert_eq!(
        runner::run(&bad, &fx.env).await.unwrap_err().exit,
        ExitCode::Usage
    );
    let lonely = RunEnv {
        store: StoreFile {
            version: 1,
            profiles: vec![],
            environments: vec![],
        },
        runs: RunsDir::new(fx.env.runs.root.clone()),
        trigger: RunTrigger::Cli,
    };
    let err = runner::run(&trusted(fx.notebook.clone()), &lonely)
        .await
        .unwrap_err();
    assert_eq!(err.exit, ExitCode::PolicyRefused);
    assert!(err.body.hint.unwrap().contains("--connection sales="));
}

#[tokio::test(flavor = "multi_thread")]
async fn exports_render_through_the_report_lane_and_redact() {
    use super::export::{ExportArgs, ExportFormat, TemplateArg, ThemeArg, export};
    let fx = fixture(vec![
        json!({"id": "c_t", "name": "tenant_orders", "kind": "sql",
        "connection_ref": "sales", "source": "SELECT count(*) AS n FROM orders WHERE region <> :tenant"}),
    ]);
    // A sensitive parameter: its value must never reach a report.
    let path = fx.notebook.clone();
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["parameters"].as_array_mut().unwrap().push(json!({
        "id": "tenant", "name": "tenant", "type": "string", "default": "ZZ", "sensitive": true}));
    std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
    let mut args = trusted(path.clone());
    args.params = vec!["tenant=acme-secret-tenant".into()];
    let run = runner::run(&args, &fx.env).await.unwrap();
    assert!(run.summary.failed.is_empty(), "{:?}", run.cells);
    let tenant = run
        .record
        .as_ref()
        .unwrap()
        .cells
        .iter()
        .find(|c| c.cell_id == "c_t")
        .unwrap();
    let bound = "SELECT count(*) AS n FROM orders WHERE region <> 'acme-secret-tenant'";
    assert_ne!(
        tenant.sql_hash.as_deref(),
        Some(sqlly_services::notebook::run_record::sql_hash(bound).as_str()),
        "the record's hash can't be checked against guesses of the secret"
    );
    assert_eq!(
        tenant.sql_hash.as_deref(),
        Some(
            sqlly_services::notebook::run_record::sql_hash(
                "SELECT count(*) AS n FROM orders WHERE region <> :tenant"
            )
            .as_str()
        )
    );
    let out_dir = path.parent().unwrap().join("out");
    for (format, name) in [
        (ExportFormat::Html, "report.html"),
        (ExportFormat::Md, "report.md"),
        (ExportFormat::Xlsx, "report.xlsx"),
        (ExportFormat::Csv, "report.csv"),
        (ExportFormat::Pdf, "report.pdf"),
    ] {
        let out = out_dir.join(name);
        let (text, data) = export(
            &ExportArgs {
                file: path.clone(),
                run: "latest".into(),
                format,
                out: out.clone(),
                template: Some(TemplateArg::Audit),
                dashboard: None,
                structure_only: false,
                no_sql: false,
                page_per_section: false,
                theme: ThemeArg::Light,
                force: false,
                accept_redactions: true,
            },
            &fx.env.runs,
        )
        .unwrap_or_else(|e| panic!("{name}: {}", e.body.message));
        assert!(text.contains("fidelity"), "{text}");
        let bytes = std::fs::read(&out).unwrap();
        assert!(!bytes.is_empty(), "{name}");
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("acme-secret-tenant"),
            "{name} leaked a sensitive value"
        );
        if format == ExportFormat::Html {
            assert!(body.contains("Orders") && body.contains("182.5"), "{name}");
        }
        assert!(data["files"].as_array().is_some_and(|f| !f.is_empty()));
    }
    // Sensitive findings need acknowledging.
    let unacknowledged = export(
        &ExportArgs {
            file: path.clone(),
            run: "latest".into(),
            format: ExportFormat::Md,
            out: out_dir.join("other.md"),
            template: None,
            dashboard: None,
            structure_only: false,
            no_sql: false,
            page_per_section: false,
            theme: ThemeArg::Light,
            force: false,
            accept_redactions: false,
        },
        &fx.env.runs,
    )
    .unwrap_err();
    assert_eq!(unacknowledged.exit, ExitCode::PolicyRefused);
    assert!(!out_dir.join("other.md").exists());
    let audit = std::fs::read_to_string(fx.env.runs.audit_dir().join("audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"export\"") && audit.contains("\"run_cell\""),
        "{audit}"
    );
    assert!(!audit.contains("acme-secret-tenant"));
    // Existing files are kept unless --force.
    let again = export(
        &ExportArgs {
            file: path.clone(),
            run: "latest".into(),
            format: ExportFormat::Html,
            out: out_dir.join("report.html"),
            template: None,
            dashboard: None,
            structure_only: true,
            no_sql: true,
            page_per_section: false,
            theme: ThemeArg::Dark,
            force: false,
            accept_redactions: true,
        },
        &fx.env.runs,
    );
    assert!(again.is_err());
    let forced = export(
        &ExportArgs {
            file: path,
            run: "latest".into(),
            format: ExportFormat::Html,
            out: out_dir.join("report.html"),
            template: None,
            dashboard: None,
            structure_only: true,
            no_sql: true,
            page_per_section: false,
            theme: ThemeArg::Dark,
            force: true,
            accept_redactions: true,
        },
        &fx.env.runs,
    );
    assert!(forced.is_ok());
    let body = std::fs::read_to_string(out_dir.join("report.html")).unwrap();
    assert!(!body.contains("182.5"), "structure-only export kept values");
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboards_export_with_their_grid_and_exports_are_audited() {
    use super::export::{ExportArgs, ExportFormat, ThemeArg, export};
    let fx = fixture(vec![
        json!({"id": "c_board", "name": "board", "kind": "layout", "spec": {
        "title": "Regional board",
        "sections": [{"id": "s1", "title": "Overview section", "tabs": [{"id": "tab1", "title": "Main",
            "tiles": [{"id": "t1", "cell": "c_orders", "title": "Regional orders tile", "span": 6},
                      {"id": "t2", "cell": "c_kpi", "title": "Revenue tile", "span": 6}]}]}]}}),
    ]);
    let run = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert!(run.summary.failed.is_empty(), "{:?}", run.cells);
    let out_dir = fx.notebook.parent().unwrap().join("board");
    let args = |dashboard: &str, out: &str, structure_only: bool| ExportArgs {
        file: fx.notebook.clone(),
        run: "latest".into(),
        format: ExportFormat::Html,
        out: out_dir.join(out),
        template: None,
        dashboard: Some(dashboard.into()),
        structure_only,
        no_sql: false,
        page_per_section: false,
        theme: ThemeArg::Light,
        force: false,
        accept_redactions: true,
    };
    let (_, data) = export(&args("board", "board.html", false), &fx.env.runs)
        .unwrap_or_else(|e| panic!("{}", e.body.message));
    assert_eq!(data["dashboard"], "c_board");
    let body = std::fs::read_to_string(out_dir.join("board.html")).unwrap();
    assert!(body.contains("Regional board"), "the dashboard's title");
    assert!(body.contains("Overview section") && body.contains("Regional orders tile"));
    assert!(
        !body.contains("Orders by region"),
        "cells the dashboard does not show stay out"
    );
    // By id too, structure only.
    export(&args("c_board", "bare.html", true), &fx.env.runs).unwrap();
    let missing = export(&args("nope", "nope.html", false), &fx.env.runs).unwrap_err();
    assert_eq!(missing.exit, ExitCode::Usage);
    assert!(
        missing.body.message.contains("board"),
        "{}",
        missing.body.message
    );

    let audit = std::fs::read_to_string(fx.env.runs.audit_dir().join("audit.jsonl")).unwrap();
    let exports: Vec<serde_json::Value> = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["action"] == "export")
        .collect();
    assert_eq!(exports.len(), 2, "{audit}");
    for event in &exports {
        assert_eq!(event["export"]["format"], "html");
        assert_eq!(event["export"]["scope"], "dashboard");
        assert_eq!(event["export"]["cells"], json!(["c_board"]));
        assert!(event["export"]["redaction"].is_object(), "{event}");
    }
    assert_eq!(exports[0]["export"]["structure_only"], false);
    assert_eq!(exports[1]["export"]["structure_only"], true);
    assert!(exports[1]["export"]["redaction"]["rows_removed"].as_u64() > Some(0));
    assert!(!audit.contains("182.5"), "no data in the audit log");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_record_errors_keep_no_secrets() {
    let fx = fixture(vec![json!({"id": "c_bad", "name": "bad", "kind": "sql",
        "connection_ref": "sales",
        "source": "SELECT * FROM \"Server=db;User Id=sa;Password=Sup3rS3cret!;\""})]);
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert_eq!(report.summary.failed, vec!["bad".to_string()]);
    let runs = fx.env.runs.list(&fx.key()).unwrap();
    let record = runs[0].cells.iter().find(|c| c.cell_id == "c_bad").unwrap();
    let error = &record.error.as_ref().unwrap().message;
    assert!(!error.contains("Sup3rS3cret"), "{error}");
    assert!(error.contains("Password"), "{error}");
    let line = serde_json::to_string(&runs[0]).unwrap();
    assert!(!line.contains("Sup3rS3cret"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_cells_import_their_files_for_local_cells() {
    let fx = fixture(vec![
        json!({"id": "c_targets", "name": "targets", "kind": "attachment",
               "spec": {"relation": "region_targets", "path": "files/targets.csv",
                        "format": "csv", "file_name": "targets.csv"}}),
        json!({"id": "c_vs", "name": "vs_target", "kind": "sql", "connection_ref": "local",
               "source": "SELECT t.region, t.target, sum(o.amount) AS total FROM {{targets}} t JOIN {{region_orders}} o ON o.region = t.region GROUP BY t.region, t.target"}),
        json!({"id": "c_pic", "name": "logo", "kind": "attachment",
               "spec": {"path": "files/logo.png", "format": "png", "file_name": "logo.png"}}),
    ]);
    let folder = fx.notebook.parent().unwrap().join("files");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("targets.csv"), "region,target\nEU,150\nUS,50\n").unwrap();
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    let line = |name: &str| report.cells.iter().find(|c| c.cell == name).unwrap();
    assert_eq!(
        line("targets").status,
        CellStatus::Fresh,
        "{:?}",
        line("targets").note
    );
    assert_eq!(line("targets").rows, Some(2));
    assert_eq!(
        line("vs_target").status,
        CellStatus::Fresh,
        "{:?}",
        line("vs_target").note
    );
    assert_eq!(line("vs_target").rows, Some(1), "region = EU");
    assert_eq!(
        line("logo").status,
        CellStatus::Unavailable,
        "pictures are app-only"
    );
    assert!(report.summary.skipped.contains(&"logo".to_string()));
    assert!(!report.summary.failed.contains(&"targets".to_string()));

    // A file elsewhere is read only after it was picked in the app here.
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().join("targets.csv");
    std::fs::write(&elsewhere, "region,target\nEU,150\n").unwrap();
    let fx = fixture(vec![
        json!({"id": "c_abs", "name": "abs_targets", "kind": "attachment",
        "spec": {"relation": "abs_targets", "absolute_path": elsewhere.to_string_lossy(),
                 "format": "csv", "file_name": "targets.csv"}}),
    ]);
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert_eq!(report.summary.failed, vec!["abs_targets".to_string()]);
    let note = report
        .cells
        .iter()
        .find(|c| c.cell == "abs_targets")
        .unwrap()
        .note
        .clone();
    assert!(note.unwrap_or_default().contains("picked"));
    let prefs = json!({"approved_files": {fx.key().scope: [elsewhere.to_string_lossy()]}});
    std::fs::write(
        fx.env.runs.local_preferences_path(),
        serde_json::to_vec(&prefs).unwrap(),
    )
    .unwrap();
    let report = runner::run(&trusted(fx.notebook.clone()), &fx.env)
        .await
        .unwrap();
    assert!(report.summary.failed.is_empty(), "{:?}", report.cells);
    let line = report
        .cells
        .iter()
        .find(|c| c.cell == "abs_targets")
        .unwrap();
    assert_eq!(line.rows, Some(1));
}
