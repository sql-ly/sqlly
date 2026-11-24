use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use super::report::{DashboardReportOptions, cell_blocks, cells_report, dashboard_report};
use super::*;
use crate::notebook::fingerprint::Fingerprint;
use crate::notebook::references::Dialects;
use crate::notebook::{CellOutput, Completeness, Provenance, RandomIds, load};
use crate::report::doc::{Block, Placement, walk};
use crate::report::{
    NotebookReportInput, RedactionPolicy, RenderOptions, ReportFormat, redact, render,
};

const FIXTURE: &str = include_str!(
    "../../../../../../../scripts/docs-shots/fixtures/notebooks/dashboard-revenue.sqllynb"
);

fn fixture() -> NotebookDoc {
    load(None, FIXTURE, &mut RandomIds, None)
        .expect("fixture loads")
        .doc
}

fn layout_of(doc: &NotebookDoc) -> LayoutSpec {
    let cell = doc
        .cells
        .iter()
        .find(|c| c.kind == CellKind::Layout)
        .unwrap();
    LayoutSpec::from_value(&cell.spec).unwrap()
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
        elapsed_ms: 4,
        run_id: "r_fixed".into(),
        fingerprint: Fingerprint("f".into()),
        provenance: Provenance {
            connection_label: "Storefront Dev".into(),
            database: Some("Northwind".into()),
            ..Provenance::default()
        },
        freshness: Default::default(),
    }
}

fn s(v: &str) -> CellValue {
    CellValue::String(v.into())
}

fn outputs() -> BTreeMap<String, CellOutput> {
    let mut out = BTreeMap::new();
    out.insert(
        "cell_totals".into(),
        output(
            &["Orders", "Customers", "Revenue"],
            vec![vec![s("691"), s("86"), s("1145990.12")]],
        ),
    );
    out.insert(
        "cell_by_country".into(),
        output(
            &["ShipCountry", "Orders", "Revenue"],
            vec![
                vec![s("USA"), s("101"), s("232300.10")],
                vec![s("Germany"), s("103"), s("217100.55")],
            ],
        ),
    );
    out.insert(
        "cell_top_customers".into(),
        output(
            &["CompanyName", "Country", "Revenue"],
            vec![vec![s("QUICK-Stop"), s("Germany"), s("110277.30")]],
        ),
    );
    out
}

fn input<'a>(
    doc: &'a NotebookDoc,
    outputs: &'a BTreeMap<String, CellOutput>,
) -> NotebookReportInput<'a> {
    static EMPTY_A: std::sync::OnceLock<
        BTreeMap<String, Vec<crate::analytics::assertions::AssertionOutcome>>,
    > = std::sync::OnceLock::new();
    static EMPTY_K: std::sync::OnceLock<BTreeMap<String, crate::analytics::kpi::KpiResult>> =
        std::sync::OnceLock::new();
    NotebookReportInput {
        doc,
        run: None,
        outputs,
        assertions: EMPTY_A.get_or_init(BTreeMap::new),
        kpis: EMPTY_K.get_or_init(BTreeMap::new),
        parameters: &[],
        connections: &[],
        generated_at: Some("2026-11-18T09:00:00Z"),
        labels: crate::report::ReportLabels::english(),
        resolve_message: crate::report::english,
    }
}

#[test]
fn the_fixture_round_trips_through_the_notebook_file() {
    let doc = fixture();
    let spec = layout_of(&doc);
    assert_eq!(spec.sections.len(), 2);
    assert_eq!(spec.tiles().count(), 8);
    assert!(spec.validate(&doc).is_empty(), "{:?}", spec.validate(&doc));
    let text = doc.to_canonical_json();
    let again = load(None, &text, &mut RandomIds, None).unwrap().doc;
    assert_eq!(layout_of(&again), spec);
    assert_eq!(again.to_canonical_json(), text, "canonical JSON is stable");
}

#[test]
fn unknown_fields_and_rules_survive_a_save() {
    let value = json!({
        "sections": [{"id": "s1", "future": 1, "tabs": [{"id": "tab1", "tiles": [
            {"id": "t1", "cell": "c", "span": 4, "glow": "soft",
             "visible_when": {"kind": "when_moon_is_full", "phase": 0.5}}
        ]}]}],
        "branding": {"font": "Inter"}
    });
    let spec = LayoutSpec::from_value(&value).unwrap();
    let tile = spec.tile("t1").unwrap();
    assert_eq!(
        tile.span,
        Spans {
            wide: 4,
            compact: 12
        },
        "a bare span is the wide span"
    );
    assert!(matches!(tile.visible_when, Some(Condition::Other(_))));
    let back = spec.to_value();
    assert_eq!(back["branding"], json!({"font": "Inter"}));
    assert_eq!(back["sections"][0]["future"], json!(1));
    assert_eq!(
        back["sections"][0]["tabs"][0]["tiles"][0]["glow"],
        json!("soft")
    );
    assert_eq!(
        back["sections"][0]["tabs"][0]["tiles"][0]["visible_when"],
        json!({"kind": "when_moon_is_full", "phase": 0.5})
    );
    let never = |_: &str| None;
    let none = |_: &str| None;
    assert!(!tile.is_visible(&VisibilityInputs {
        param: &never,
        test_passed: &none
    }));
}

#[test]
fn unreadable_specs_are_reported_not_replaced() {
    let err = LayoutSpec::from_value(&json!({"sections": "oops"})).unwrap_err();
    assert_eq!(
        err.message().key(),
        Some("notebook.dashboard.issue.unreadable")
    );
    assert_eq!(
        LayoutSpec::from_value(&serde_json::Value::Null).unwrap(),
        LayoutSpec::default()
    );
}

#[test]
fn validation_finds_unknown_cells_bad_spans_and_targets() {
    let mut doc = fixture();
    let mut spec = layout_of(&doc);
    let layout_id = doc
        .cells
        .iter()
        .find(|c| c.kind == CellKind::Layout)
        .unwrap()
        .id
        .clone();
    let (s, t) = (0, 0);
    let ghost = spec.add_tile(s, t, "cell_gone");
    let itself = spec.add_tile(s, t, &layout_id);
    spec.tile_mut(&ghost).unwrap().span.wide = 13;
    spec.tile_mut(&ghost).unwrap().height = 0;
    spec.tile_mut(&itself).unwrap().drill = Some(Drill {
        section: "nowhere".into(),
        ..Drill::default()
    });
    spec.tile_mut(&itself).unwrap().visible_when = Some(Condition::Known(Visibility::TestPasses {
        cell: "cell_totals".into(),
    }));
    spec.filters.params.push("region".into());
    spec.sections.push(Section {
        id: "s1".into(),
        ..Section::default()
    });
    let issues = spec.validate(&doc);
    for expected in [
        LayoutIssue::UnknownCell {
            tile: ghost.clone(),
            cell: "cell_gone".into(),
        },
        LayoutIssue::TileShowsDashboard {
            tile: itself.clone(),
        },
        LayoutIssue::SpanOutOfRange {
            tile: ghost.clone(),
            span: 13,
        },
        LayoutIssue::HeightOutOfRange {
            tile: ghost.clone(),
            height: 0,
        },
        LayoutIssue::UnknownDrillTarget {
            tile: itself.clone(),
            section: "nowhere".into(),
        },
        LayoutIssue::UnknownCheck {
            tile: itself.clone(),
            cell: "cell_totals".into(),
        },
        LayoutIssue::UnknownParameter {
            param: "region".into(),
        },
        LayoutIssue::DuplicateId { id: "s1".into() },
        LayoutIssue::SectionWithoutTabs {
            section: "s1".into(),
        },
    ] {
        assert!(issues.contains(&expected), "{expected:?} in {issues:?}");
    }
    for issue in &issues {
        assert!(issue.message().key().is_some());
    }
    assert!(spec.normalize());
    assert_eq!(spec.tile(&ghost).unwrap().span.wide, 12);
    assert_eq!(spec.tile(&ghost).unwrap().height, MIN_HEIGHT);
    doc.cells.retain(|c| c.id != "cell_by_country");
    assert!(
        layout_of(&fixture()).validate(&doc).iter().any(
            |i| matches!(i, LayoutIssue::UnknownCell { cell, .. } if cell == "cell_by_country")
        ),
        "deleting a cell a tile shows is reported"
    );
}

#[test]
fn editing_operations_keep_ids_unique_and_spans_in_range() {
    let doc = fixture();
    let mut spec = LayoutSpec::starter(&doc, 3);
    assert_eq!(spec.sections.len(), 1);
    assert_eq!(spec.tiles().count(), 3);
    assert!(
        spec.sections[0].title.is_empty(),
        "no translated text is saved"
    );
    let (s2, _) = spec.add_section();
    let tab = spec.add_tab(s2).unwrap();
    assert_eq!(tab, 1);
    let t = spec.add_tile(s2, tab, "cell_late");
    assert!(spec.validate(&doc).is_empty(), "{:?}", spec.validate(&doc));
    assert!(spec.set_span(&t, Breakpoint::Wide, 40));
    assert_eq!(spec.tile(&t).unwrap().span(Breakpoint::Wide), 12);
    assert!(
        !spec.set_span(&t, Breakpoint::Wide, 12),
        "no change, no edit"
    );
    assert!(spec.set_height(&t, 99));
    assert_eq!(spec.tile(&t).unwrap().height, MAX_HEIGHT);
    let first = spec.sections[0].tabs[0].tiles[0].id.clone();
    assert!(spec.move_tile(&first, 1));
    assert_eq!(spec.sections[0].tabs[0].tiles[1].id, first);
    assert!(!spec.move_tile(&first, 5));
    assert!(spec.move_tile_to(&first, SlideRef { section: s2, tab }));
    assert_eq!(spec.tile_pos(&first).unwrap().section, s2);
    assert!(spec.remove_tile(&first));
    assert!(spec.tile(&first).is_none());
    assert!(!spec.remove_tab(0, 0), "a section keeps its last tab");
    assert!(spec.remove_tab(s2, tab));
    let mut ids: Vec<String> = spec.tiles().map(|(_, t)| t.id.clone()).collect();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before);
}

#[test]
fn rows_pack_to_twelve_columns_per_breakpoint() {
    let spec = layout_of(&fixture());
    let tiles: Vec<&Tile> = spec.sections[0].tabs[0].tiles.iter().collect();
    assert_eq!(
        pack_rows(&tiles, Breakpoint::Wide),
        vec![vec![0], vec![1, 2], vec![3]]
    );
    assert_eq!(
        pack_rows(&tiles, Breakpoint::Compact),
        vec![vec![0], vec![1], vec![2], vec![3]]
    );
    assert_eq!(Breakpoint::for_width(600.0), Breakpoint::Compact);
    assert_eq!(Breakpoint::for_width(1200.0), Breakpoint::Wide);
}

#[test]
fn visibility_follows_parameters_and_checks() {
    let spec = layout_of(&fixture());
    let tile = spec.tile("t7").unwrap();
    let unset = |_: &str| Some(String::new());
    let set = |name: &str| (name == "country").then(|| "Germany".to_string());
    let none = |_: &str| None;
    assert!(!tile.is_visible(&VisibilityInputs {
        param: &unset,
        test_passed: &none
    }));
    assert!(tile.is_visible(&VisibilityInputs {
        param: &set,
        test_passed: &none
    }));
    let mut check = Tile::new("x", "c");
    check.visible_when = Some(Condition::Known(Visibility::TestPasses {
        cell: "a".into(),
    }));
    let passed = |_: &str| Some(true);
    let failed = |_: &str| Some(false);
    for (outcome, shown) in [
        (&passed as &dyn Fn(&str) -> Option<bool>, true),
        (&failed, false),
        (&none, false),
    ] {
        assert_eq!(
            check.is_visible(&VisibilityInputs {
                param: &unset,
                test_passed: outcome
            }),
            shown
        );
    }
    let mut equals = Tile::new("y", "c");
    equals.visible_when = Some(Condition::Known(Visibility::ParamEquals {
        param: "country".into(),
        value: "USA".into(),
    }));
    assert!(!equals.is_visible(&VisibilityInputs {
        param: &set,
        test_passed: &none
    }));
}

#[test]
fn slides_and_drill_targets() {
    let mut spec = layout_of(&fixture());
    assert_eq!(
        spec.slides(),
        vec![
            SlideRef { section: 0, tab: 0 },
            SlideRef { section: 0, tab: 1 },
            SlideRef { section: 1, tab: 0 },
        ]
    );
    let drill = spec.tile("t2").unwrap().drill.clone().unwrap();
    assert_eq!(
        spec.drill_target(&drill),
        Some(SlideRef { section: 1, tab: 0 })
    );
    assert_eq!(spec.parameters(), vec!["country", "min_year"]);
    spec.presentation.slides = SlideMode::Whole;
    assert_eq!(spec.slides(), vec![SlideRef { section: 0, tab: 0 }]);
}

#[test]
fn refresh_lists_the_remote_cells_behind_tiles() {
    let doc = fixture();
    let graph = Graph::build(&doc, &Dialects::single(DatabaseType::SqlServer));
    let cells = remote_cells_behind(&doc, &graph, &["cell_by_country", "cell_note"]);
    assert_eq!(cells, vec!["cell_orders", "cell_by_country"]);
}

#[test]
fn logos_are_checked() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#;
    let logo = Logo::from_file("SVG", svg, "Acme").unwrap();
    assert_eq!(logo.format, LogoFormat::Svg);
    assert!(logo.bytes().is_some());
    assert_eq!(
        Logo::from_file("svg", br#"<svg><script>alert(1)</script></svg>"#, "").unwrap_err(),
        LogoError::Unsafe
    );
    assert_eq!(
        Logo::from_file("png", b"not a png", "").unwrap_err(),
        LogoError::Unsupported
    );
    assert_eq!(
        Logo::from_file("gif", b"GIF89a", "").unwrap_err(),
        LogoError::Unsupported
    );
    assert_eq!(
        Logo::from_file("png", &vec![0u8; MAX_LOGO_BYTES + 1], "").unwrap_err(),
        LogoError::TooLarge
    );
    let tampered = Logo {
        format: LogoFormat::Svg,
        data: "<svg onload=\"x()\"></svg>".into(),
        alt: String::new(),
    };
    assert!(
        tampered.bytes().is_none(),
        "a hand-edited logo is checked again"
    );
    for e in [
        LogoError::TooLarge,
        LogoError::Unsupported,
        LogoError::Unsafe,
    ] {
        assert!(e.message().key().is_some());
    }
}

fn columns_spans(blocks: &[Block]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    walk(blocks, &mut |b| {
        if let Block::Columns(c) = b {
            out.push(c.columns.iter().map(|c| c.span).collect());
        }
    });
    out
}

#[test]
fn the_dashboard_report_keeps_the_grid_and_hides_hidden_tiles() {
    let doc = fixture();
    let outputs = outputs();
    let report = dashboard_report(
        &input(&doc, &outputs),
        "cell_dashboard",
        DashboardReportOptions::default(),
    )
    .unwrap();
    assert_eq!(report.meta.title, "Northwind sales dashboard");
    assert_eq!(
        columns_spans(&report.blocks),
        vec![
            vec![12],
            vec![7, 5],
            vec![12],
            vec![6, 6],
            vec![12],
            vec![12]
        ],
        "a country is picked, so its detail tile shows"
    );
    let mut tables = Vec::new();
    let mut code = 0;
    walk(&report.blocks, &mut |b| match b {
        Block::Table(t) => tables.push(t.cell_id.clone().unwrap()),
        Block::Code(_) => code += 1,
        _ => {}
    });
    assert_eq!(
        tables,
        vec!["cell_totals", "cell_by_country", "cell_top_customers"]
    );
    assert!(code >= 5, "each SQL tile carries its SQL for the appendix");
    assert!(!report.blocks.iter().any(|b| matches!(b, Block::PageBreak)));

    let mut doc = doc;
    doc.parameters[0].default = None;
    let paged = dashboard_report(
        &input(&doc, &outputs),
        "cell_dashboard",
        DashboardReportOptions {
            page_per_section: true,
        },
    )
    .unwrap();
    assert_eq!(
        columns_spans(&paged.blocks).len(),
        5,
        "the country tile hides when no country is set"
    );
    assert_eq!(
        paged
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::PageBreak))
            .count(),
        1
    );
    assert!(dashboard_report(&input(&doc, &outputs), "cell_totals", Default::default()).is_none());
}

#[test]
fn cell_blocks_match_the_whole_notebook_report() {
    let doc = fixture();
    let outputs = outputs();
    let input = input(&doc, &outputs);
    let whole = crate::report::from_notebook(&input);
    let pieced: Vec<Block> = doc
        .cells
        .iter()
        .flat_map(|c| cell_blocks(&input, &c.id))
        .collect();
    assert_eq!(pieced, whole.blocks);
    let picked = cells_report(&input, &["cell_by_country".to_string()]);
    assert!(matches!(picked.blocks[0], Block::Heading(_)));
    assert!(
        picked.blocks.iter().any(
            |b| matches!(b, Block::Table(t) if t.cell_id.as_deref() == Some("cell_by_country"))
        )
    );
    assert_eq!(picked.meta.title, whole.meta.title);
}

#[test]
fn structure_only_and_sql_appendix_apply_to_dashboards() {
    let doc = fixture();
    let outputs = outputs();
    let mut report =
        dashboard_report(&input(&doc, &outputs), "cell_dashboard", Default::default()).unwrap();
    report.meta.layout.sql = Placement::Appendix;
    let (stripped, summary) = redact(&report, &RedactionPolicy::structure_only());
    assert!(summary.rows_removed >= 4);
    walk(&stripped.blocks, &mut |b| {
        if let Block::Table(t) = b {
            assert!(t.rows.is_empty());
        }
    });
    let html = render(&report, ReportFormat::Html, &RenderOptions::default()).unwrap();
    let text = String::from_utf8(html.files[0].bytes.clone()).unwrap();
    let appendix = text.find("class=\"appendix\"").expect("SQL appendix");
    assert!(text[appendix..].contains("GROUP BY ShipCountry"));
    assert!(
        text.contains("class=\"span-7\""),
        "the grid carries into HTML"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn dashboard_pdfs_are_byte_for_byte_repeatable_with_a_page_per_section() {
    let doc = fixture();
    let outputs = outputs();
    let report = dashboard_report(
        &input(&doc, &outputs),
        "cell_dashboard",
        DashboardReportOptions {
            page_per_section: true,
        },
    )
    .unwrap();
    let options = RenderOptions::default();
    let a = render(&report, ReportFormat::Pdf, &options).unwrap();
    let b = render(&report, ReportFormat::Pdf, &options).unwrap();
    assert_eq!(a.files[0].bytes, b.files[0].bytes);
    let flowing = dashboard_report(
        &input(&doc, &outputs),
        "cell_dashboard",
        DashboardReportOptions::default(),
    )
    .unwrap();
    let pages = |r: &crate::report::ReportDoc| {
        let out = render(r, ReportFormat::Pdf, &options).unwrap();
        lopdf::Document::load_mem(&out.files[0].bytes)
            .unwrap()
            .get_pages()
            .len()
    };
    assert_eq!(
        pages(&report),
        pages(&flowing) + 1,
        "each section starts on its own page"
    );
}

#[test]
fn every_message_is_in_the_catalog_with_the_same_english_text() {
    const EN: &str = include_str!("../../../../../sqlly-i18n/translations/en.json");
    let catalog: serde_json::Value = serde_json::from_str(EN).unwrap();
    let x = || "x".to_string();
    let mut messages: Vec<sqlly_core::DisplayMessage> = [
        LayoutIssue::UnknownCell {
            tile: x(),
            cell: x(),
        },
        LayoutIssue::TileShowsDashboard { tile: x() },
        LayoutIssue::SpanOutOfRange {
            tile: x(),
            span: 13,
        },
        LayoutIssue::HeightOutOfRange {
            tile: x(),
            height: 9,
        },
        LayoutIssue::DuplicateId { id: x() },
        LayoutIssue::EmptyId,
        LayoutIssue::SectionWithoutTabs { section: x() },
        LayoutIssue::UnknownParameter { param: x() },
        LayoutIssue::UnknownDrillTarget {
            tile: x(),
            section: x(),
        },
        LayoutIssue::UnknownCheck {
            tile: x(),
            cell: x(),
        },
        LayoutIssue::UnsupportedCondition { tile: x() },
    ]
    .iter()
    .map(LayoutIssue::message)
    .collect();
    messages.extend(
        [
            LogoError::TooLarge,
            LogoError::Unsupported,
            LogoError::Unsafe,
        ]
        .map(LogoError::message),
    );
    messages.push(LayoutReadError::Invalid { detail: x() }.message());
    for m in &messages {
        let key = m.key().expect("keyed");
        let mut node = &catalog;
        for part in key.split('.') {
            node = node
                .get(part)
                .unwrap_or_else(|| panic!("{key} missing from en.json"));
        }
        let mut text = node["value"].as_str().unwrap().to_string();
        for (name, value) in m.arguments() {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        assert_eq!(text, m.fallback(), "{key}");
    }
}
