//! The export pipeline per option (scope, layout, SQL placement, data
//! permission, saved runs), the sensitive-data confirmation, sanitized
//! notebook copies, deterministic PDFs and atomic writes.

use std::sync::Arc;

use gpui::TestAppContext;
use serde_json::json;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::notebook::fingerprint::Fingerprint;
use sqlly_services::notebook::params::ParamScope;
use sqlly_services::notebook::run_record::{RunTrigger, doc_hash};
use sqlly_services::notebook::{Completeness, Provenance};
use sqlly_services::report::doc::{Block, walk};

use super::*;
use crate::notebook::dashboard::tests::{REVENUE, WRITES};

fn fixture(text: &str) -> NotebookDoc {
    crate::notebook::files::load_file("/tmp/export.sqllynb", text, None)
        .unwrap()
        .doc
}

fn output(columns: &[&str], rows: Vec<Vec<CellValue>>) -> CellOutput {
    let n = rows.len() as u64;
    CellOutput {
        columns: columns
            .iter()
            .map(|c| ResultColumn {
                name: (*c).into(),
                data_type: "nvarchar".into(),
                nullable: true,
            })
            .collect(),
        rows: Arc::new(rows),
        completeness: Completeness::Complete,
        row_count: n,
        elapsed_ms: 1,
        run_id: "r_1".into(),
        fingerprint: Fingerprint("f".into()),
        provenance: Provenance {
            connection_label: "Storefront Dev".into(),
            ..Provenance::default()
        },
        freshness: Default::default(),
    }
}

fn s(v: &str) -> CellValue {
    CellValue::String(v.into())
}

fn snapshot(doc: NotebookDoc) -> ExportSnapshot {
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "cell_by_country".to_string(),
        output(
            &["ShipCountry", "Revenue"],
            vec![
                vec![s("USA"), s("232300.10")],
                vec![s("Germany"), s("217100.55")],
            ],
        ),
    );
    outputs.insert(
        "cell_totals".to_string(),
        output(
            &["Orders", "Revenue"],
            vec![vec![s("691"), s("1145990.12")]],
        ),
    );
    ExportSnapshot {
        doc,
        outputs,
        connections: vec![ConnectionLabel {
            label: "Storefront Dev".into(),
            ..ConnectionLabel::default()
        }],
        dashboards: vec![("cell_dashboard".into(), "dashboard".into())],
        selected: vec!["cell_by_country".into()],
        runs: Vec::new(),
        trusted: true,
        directory: None,
    }
}

fn report_of(preview: &ExportPreview) -> &ReportDoc {
    preview.report.as_ref().expect("a report format")
}

fn blocks(report: &ReportDoc, pred: impl Fn(&Block) -> bool) -> Vec<&Block> {
    let mut out = Vec::new();
    walk(&report.blocks, &mut |b| {
        if pred(b) {
            out.push(b);
        }
    });
    out
}

fn html(preview: &ExportPreview) -> String {
    String::from_utf8(preview.files[0].bytes.clone()).unwrap()
}

#[test]
fn each_option_builds_the_matching_report() {
    let snap = snapshot(fixture(REVENUE));
    let base = ExportOptions::default();

    let full = build_export(&snap, &base, false).unwrap();
    assert_eq!(full.decision, ExportDecision::Allowed);
    assert_eq!(report_of(&full).meta.layout.sql, Placement::Appendix);
    assert!(html(&full).contains("class=\"appendix\""));
    let tables = blocks(report_of(&full), |b| matches!(b, Block::Table(_)));
    assert!(
        tables
            .iter()
            .any(|b| matches!(b, Block::Table(t) if t.rows.len() == 2))
    );

    let structure = build_export(
        &snap,
        &ExportOptions {
            data: DataPermission::StructureOnly,
            ..base.clone()
        },
        false,
    )
    .unwrap();
    assert!(report_of(&structure).meta.data_withheld);
    for table in blocks(report_of(&structure), |b| matches!(b, Block::Table(_))) {
        let Block::Table(t) = table else {
            unreachable!()
        };
        assert!(t.rows.is_empty(), "structure-only drops every row");
    }
    assert!(!html(&structure).contains("232300.10"));
    assert!(
        structure.removed.iter().any(|r| r.contains('3')),
        "{:?}",
        structure.removed
    );

    let no_sql = build_export(
        &snap,
        &ExportOptions {
            sql: SqlPlacement::Omit,
            provenance: false,
            ..base.clone()
        },
        false,
    )
    .unwrap();
    assert_eq!(report_of(&no_sql).meta.layout.sql, Placement::Omit);
    assert_eq!(report_of(&no_sql).meta.layout.provenance, Placement::Omit);
    assert!(!html(&no_sql).contains("GROUP BY ShipCountry"));
    assert!(no_sql.redaction.sql_removed > 0);

    let inline = build_export(
        &snap,
        &ExportOptions {
            sql: SqlPlacement::Inline,
            ..base.clone()
        },
        false,
    )
    .unwrap();
    assert_eq!(report_of(&inline).meta.layout.sql, Placement::Inline);
    assert!(!html(&inline).contains("class=\"appendix\""));

    let picked = build_export(
        &snap,
        &ExportOptions {
            scope: ExportScope::Selected(vec!["cell_by_country".into()]),
            ..base.clone()
        },
        false,
    )
    .unwrap();
    let ids: Vec<_> = blocks(report_of(&picked), |b| matches!(b, Block::Table(_)))
        .into_iter()
        .filter_map(|b| match b {
            Block::Table(t) => t.cell_id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["cell_by_country"]);
}

#[test]
fn dashboards_keep_their_grid_under_any_layout() {
    let snap = snapshot(fixture(REVENUE));
    for template in [
        None,
        Some(TemplateKind::Executive),
        Some(TemplateKind::Financial),
    ] {
        let preview = build_export(
            &snap,
            &ExportOptions {
                scope: ExportScope::Dashboard("cell_dashboard".into()),
                template,
                ..ExportOptions::default()
            },
            false,
        )
        .unwrap();
        let report = report_of(&preview);
        assert!(
            !blocks(
                report,
                |b| matches!(b, Block::Columns(c) if c.columns.len() == 2)
            )
            .is_empty(),
            "{template:?} keeps the 7 + 5 row"
        );
        assert!(html(&preview).contains("class=\"span-7\""));
        if let Some(kind) = template {
            assert_eq!(report.meta.template, Some(kind));
        }
    }
    let whole = build_export(
        &snap,
        &ExportOptions {
            template: Some(TemplateKind::Executive),
            ..ExportOptions::default()
        },
        false,
    )
    .unwrap();
    assert!(
        blocks(report_of(&whole), |b| matches!(b, Block::Columns(_))).is_empty(),
        "the whole-notebook export has no dashboard grid"
    );
}

#[test]
fn sensitive_data_needs_a_go_ahead_and_is_listed() {
    let mut doc = fixture(REVENUE);
    doc.parameters[0].sensitive = true;
    doc.parameters[0].default = Some(json!("Germany"));
    let snap = snapshot(doc);
    let first = build_export(&snap, &ExportOptions::default(), false).unwrap();
    assert!(matches!(
        first.decision,
        ExportDecision::NeedsConfirmation(_)
    ));
    assert!(
        first.removed.iter().any(|r| r.contains("country")),
        "the redaction summary names the parameter: {:?}",
        first.removed
    );
    assert!(!html(&first).contains("Germany\""));
    let confirmed = build_export(&snap, &ExportOptions::default(), true).unwrap();
    assert_eq!(confirmed.decision, ExportDecision::Allowed);
}

#[test]
fn exports_are_audited_with_how_they_were_made_and_no_data() {
    let mut doc = fixture(REVENUE);
    doc.parameters[0].sensitive = true;
    doc.parameters[0].default = Some(json!("Germany"));
    let snap = snapshot(doc);
    let options = ExportOptions {
        format: ExportFormat::Pdf,
        scope: ExportScope::Dashboard("cell_dashboard".into()),
        data: DataPermission::StructureOnly,
        ..ExportOptions::default()
    };
    let preview = build_export(&snap, &options, true).unwrap();
    let event = export_audit_event(&snap, &options, &preview, true, None);
    assert_eq!(event.action, AuditAction::Export);
    assert_eq!(event.decision, AuditDecision::Approved);
    assert_eq!(event.outcome, Some(AuditOutcome::Completed));
    assert_eq!(event.notebook_id, snap.doc.notebook_id);
    let export = event.export.clone().unwrap();
    assert_eq!(export.format, "pdf");
    assert_eq!(export.scope, ExportScopeKind::Dashboard);
    assert_eq!(export.cells, vec!["cell_dashboard".to_string()]);
    assert!(export.structure_only);
    assert!(export.redaction.as_ref().unwrap().rows_removed > 0);
    assert_eq!(
        export.findings,
        vec!["notebook.security.export.sensitive_parameter_removed".to_string()]
    );
    let line = event.to_json_line();
    for value in ["Germany", "Brazil", "217100.55"] {
        assert!(
            !line.contains(value),
            "{value} reached the audit log: {line}"
        );
    }

    let copy = ExportOptions {
        format: ExportFormat::Notebook,
        ..ExportOptions::default()
    };
    let preview = build_export(&snap, &copy, false).unwrap();
    let failed = export_audit_event(&snap, &copy, &preview, false, Some("disk full"));
    assert_eq!(failed.decision, AuditDecision::Allow);
    assert_eq!(failed.outcome, Some(AuditOutcome::Failed));
    let export = failed.export.unwrap();
    assert_eq!(
        (export.format.as_str(), export.scope),
        ("sqllynb", ExportScopeKind::Whole)
    );
    assert!(!export.structure_only && export.redaction.is_none());
}

#[test]
fn notebook_copies_are_sanitized() {
    let mut doc = fixture(REVENUE);
    doc.extra
        .insert("sqlly_origin".into(), json!("machine-hash"));
    doc.connection_refs[0].profile_id = Some("6b1f3c9e-6a55-4b7e-9b0b-3f8f2d1c0a11".into());
    doc.parameters[1].scope = ParamScope::Personal;
    doc.cells[1]
        .extra
        .insert("outputs".into(), json!([{"rows": [[1]]}]));
    doc.cells[1].source = "SELECT 1 -- Server=db;User Id=sa;Password=Hunter2-pass;".into();
    let (clean, notes) = sanitize_notebook(&doc);
    assert!(!clean.extra.contains_key("sqlly_origin"), "no trust marker");
    assert_eq!(clean.connection_refs[0].profile_id, None);
    assert_eq!(
        clean.connection_refs[0].label, "Storefront Dev",
        "names kept"
    );
    assert_eq!(clean.parameters[1].default, None, "personal values gone");
    assert!(!clean.cells[1].extra.contains_key("outputs"));
    assert!(!clean.cells[1].source.contains("Hunter2"));
    assert!(notes.len() >= 3);
    assert!(
        clean.cell("cell_dashboard").is_some(),
        "dashboards travel with the copy"
    );

    let snap = snapshot(doc);
    let copy = build_export(
        &snap,
        &ExportOptions {
            format: ExportFormat::Notebook,
            ..ExportOptions::default()
        },
        false,
    )
    .unwrap();
    let text = String::from_utf8(copy.files[0].bytes.clone()).unwrap();
    assert!(copy.files[0].path.ends_with(".sqllynb"));
    assert!(!text.contains("6b1f3c9e") && !text.contains("machine-hash"));
    let back = crate::notebook::files::load_file("/tmp/copy.sqllynb", &text, None).unwrap();
    assert_eq!(back.doc.cells.len(), snap.doc.cells.len());

    let jupyter = build_export(
        &snap,
        &ExportOptions {
            format: ExportFormat::Jupyter,
            ..ExportOptions::default()
        },
        false,
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&jupyter.files[0].bytes).contains("\"nbformat\""));
    assert!(jupyter.notes.len() >= 2, "Jupyter's losses are listed");
}

#[test]
fn write_cells_do_not_block_static_exports() {
    let snap = snapshot(fixture(WRITES));
    let preview = build_export(&snap, &ExportOptions::default(), false).unwrap();
    assert_eq!(preview.decision, ExportDecision::Allowed);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn dashboard_pdfs_are_repeatable() {
    let snap = snapshot(fixture(REVENUE));
    let options = ExportOptions {
        format: ExportFormat::Pdf,
        scope: ExportScope::Dashboard("cell_dashboard".into()),
        page_per_section: true,
        ..ExportOptions::default()
    };
    let a = build_export(&snap, &options, false).unwrap();
    let b = build_export(&snap, &options, false).unwrap();
    assert!(a.files[0].bytes.starts_with(b"%PDF"));
    assert_eq!(a.files[0].bytes, b.files[0].bytes);
    assert!(
        a.notes.iter().any(|n| !n.is_empty()),
        "the fidelity notes come with it"
    );
}

#[test]
fn files_are_written_atomically_next_to_the_chosen_path() {
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("report.md");
    let files = vec![
        RenderedFile {
            path: "report.md".into(),
            media_type: "text/markdown",
            bytes: b"# Report\n".to_vec(),
        },
        RenderedFile {
            path: "assets/chart-1.svg".into(),
            media_type: "image/svg+xml",
            bytes: b"<svg/>".to_vec(),
        },
        RenderedFile {
            path: "../escape.txt".into(),
            media_type: "text/plain",
            bytes: b"no".to_vec(),
        },
    ];
    let written = write_files(&primary, &files).unwrap();
    assert_eq!(written.len(), 2, "paths that climb out are skipped");
    assert_eq!(std::fs::read(&primary).unwrap(), b"# Report\n");
    assert_eq!(
        std::fs::read(dir.path().join("assets").join("chart-1.svg")).unwrap(),
        b"<svg/>"
    );
    assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp files left behind");

    // A failed write leaves the existing file as it was.
    let blocked = dir.path().join("taken");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(dir.path().join("keep.md"), b"old").unwrap();
    assert!(write_files(&blocked, &files[..1]).is_err());
    assert_eq!(std::fs::read(dir.path().join("keep.md")).unwrap(), b"old");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn saved_runs_are_offered_with_their_outputs() {
    use sqlly_services::notebook::{JsonlRunStore, RunStore as _};
    let root = tempfile::tempdir().unwrap();
    let doc = fixture(REVENUE);
    let record = RunRecord {
        run_id: "r_saved".into(),
        notebook_id: doc.notebook_id.clone(),
        doc_hash: doc_hash(&doc),
        started_at: chrono::DateTime::from_timestamp(1_763_456_400, 0).unwrap(),
        ended_at: None,
        trigger: RunTrigger::Cli,
        identity_label: String::new(),
        engine_version: String::new(),
        cells: Vec::new(),
        extra: Default::default(),
    };
    let stores = crate::notebook::history::store::HistoryStores::open(
        root.path(),
        &sqlly_services::notebook_security::MemoryCacheKey::with_key(
            sqlly_services::notebook_security::cache::MasterKey::from_bytes([5; 32]),
        ),
    );
    let dir = stores.runs_dir(&doc.notebook_id);
    JsonlRunStore::new(&dir).append(&record).unwrap();
    std::fs::create_dir_all(dir.join("outputs")).unwrap();
    stores
        .put_run_output(
            &doc.notebook_id,
            None,
            "r_saved",
            "cell_by_country",
            &output(
                &["ShipCountry", "Revenue"],
                vec![vec![s("Brazil"), s("1.00")]],
            ),
        )
        .unwrap();
    std::fs::write(
        dir.join("outputs").join("r_saved.json"),
        serde_json::to_string(&json!({"run_id": "r_saved", "cells": ["cell_by_country"]})).unwrap(),
    )
    .unwrap();
    let runs = saved_runs(&stores, &doc.notebook_id, &doc.notebook_id, &|_, _| true);
    assert!(runs[0].outputs.contains_key("cell_by_country"));
    assert_eq!(runs.len(), 1);
    let mut snap = snapshot(doc);
    snap.runs = runs;
    let preview = build_export(
        &snap,
        &ExportOptions {
            run: RunSource::Saved("r_saved".into()),
            ..ExportOptions::default()
        },
        false,
    )
    .unwrap();
    assert!(html(&preview).contains("Brazil"));
    assert!(
        !html(&preview).contains("217100.55"),
        "not the current results"
    );
    assert!(
        build_export(
            &snap,
            &ExportOptions {
                run: RunSource::Saved("r_gone".into()),
                ..ExportOptions::default()
            },
            false
        )
        .is_err()
    );
}

#[test]
fn runs_the_app_recorded_are_offered_too() {
    use sqlly_services::notebook::run_record::CellRunRecord;
    use sqlly_services::notebook::{CellStatus as RunCellStatus, ConnectionIdentity};
    let root = tempfile::tempdir().unwrap();
    let doc = fixture(REVENUE);
    let identity = ConnectionIdentity {
        profile_id: "p_dev".into(),
        login_identity: "ana".into(),
        ..ConnectionIdentity::default()
    };
    let mut details = crate::notebook::history::RunDetails::default();
    details
        .identities
        .insert("cell_by_country".into(), identity.clone());
    let mut extra = sqlly_services::notebook::document::Extra::default();
    extra.insert(
        crate::notebook::history::DETAILS_KEY.to_string(),
        serde_json::to_value(&details).unwrap(),
    );
    let cell = |id: &str| CellRunRecord {
        cell_id: id.into(),
        fingerprint: Fingerprint("f".into()),
        status: RunCellStatus::Fresh,
        started_at: None,
        ended_at: None,
        row_count: Some(1),
        completeness: None,
        reused: false,
        sql_hash: None,
        error: None,
        params: Vec::new(),
    };
    let record = RunRecord {
        run_id: "r_app".into(),
        notebook_id: doc.notebook_id.clone(),
        doc_hash: doc_hash(&doc),
        started_at: chrono::DateTime::from_timestamp(1_763_456_400, 0).unwrap(),
        ended_at: None,
        trigger: RunTrigger::Manual,
        identity_label: String::new(),
        engine_version: String::new(),
        cells: vec![cell("cell_by_country"), cell("cell_totals")],
        extra,
    };
    let stores = crate::notebook::history::store::HistoryStores::open(
        root.path(),
        &sqlly_services::notebook_security::MemoryCacheKey::with_key(
            sqlly_services::notebook_security::cache::MasterKey::from_bytes([6; 32]),
        ),
    );
    // What the app writes: the record and each output under who ran it,
    // with no outputs index.
    stores.append(&doc.notebook_id, &record).unwrap();
    stores
        .put_run_output(
            &doc.notebook_id,
            Some(&identity),
            "r_app",
            "cell_by_country",
            &output(
                &["ShipCountry", "Revenue"],
                vec![vec![s("Chile"), s("2.00")]],
            ),
        )
        .unwrap();
    let runs = saved_runs(&stores, &doc.notebook_id, &doc.notebook_id, &|cell, id| {
        cell == "cell_by_country" && id == Some(&identity)
    });
    assert_eq!(runs.len(), 1, "the app's run is listed");
    assert_eq!(
        runs[0].outputs.keys().collect::<Vec<_>>(),
        vec!["cell_by_country"]
    );
    let mut snap = snapshot(doc.clone());
    snap.runs = runs;
    let preview = build_export(
        &snap,
        &ExportOptions {
            run: RunSource::Saved("r_app".into()),
            ..ExportOptions::default()
        },
        false,
    )
    .unwrap();
    assert!(html(&preview).contains("Chile"));
    // Rows recorded under another sign-in are never offered.
    assert!(saved_runs(&stores, &doc.notebook_id, &doc.notebook_id, &|_, _| false).is_empty());
}

#[gpui::test]
fn the_dialog_previews_and_asks_before_writing_sensitive_data(cx: &mut TestAppContext) {
    crate::notebook::dashboard::tests::init(cx);
    let mut doc = fixture(REVENUE);
    doc.parameters[0].sensitive = true;
    doc.parameters[0].default = Some(json!("Germany"));
    let snap = snapshot(doc);
    let (dialog, cx) =
        cx.add_window_view(|_, _| NotebookExportDialog::new(snap, ExportOptions::default()));
    cx.run_until_parked();
    dialog.read_with(cx, |d, _| {
        let preview = d.preview().unwrap().as_ref().unwrap();
        assert!(matches!(
            preview.decision,
            ExportDecision::NeedsConfirmation(_)
        ));
        assert!(!preview.notes.is_empty() || preview.files.len() == 1);
    });
    dialog.update(cx, |d, cx| d.export(cx));
    assert!(dialog.read_with(cx, |d, _| d.needs_confirmation()));
    dialog.update(cx, |d, cx| {
        d.set_options(|o| o.format = ExportFormat::Markdown, cx)
    });
    assert!(
        !dialog.read_with(cx, |d, _| d.needs_confirmation()),
        "changing an option starts over"
    );
    cx.run_until_parked();
}
