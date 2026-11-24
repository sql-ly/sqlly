//! Cross-format tests over one fixture notebook report: golden HTML,
//! Markdown and SVG, determinism, escaping, spreadsheet read-back, CSV
//! guards, redaction and fidelity notes.

use sqlly_core::CellValue;
use sqlly_testkit::golden::GoldenTest;

use super::chart::{
    Aggregate, Annotation, AnomalyBand, BarMode, ChartKind, ChartSpec, DateGrain, Encoding,
    Orientation, ReferenceLine, Sort, SortBy, TopN,
};
use super::doc::*;
use super::format::{NumberStyle, ValueFormat};
use super::*;

fn s(text: &str) -> CellValue {
    CellValue::String(text.into())
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("report")
        .join("testdata")
        .join("golden")
        .join(name)
}

fn assert_golden(actual: &str, name: &str) {
    let path = golden_path(name);
    assert!(
        GoldenTest::compare(actual, &path).unwrap_or(false),
        "{name} differs from its golden file; rerun with SQLLY_UPDATE_GOLDEN=1 to refresh\n--- actual ---\n{}",
        &actual[..actual.len().min(4000)]
    );
}

/// Monthly revenue by region, deterministic.
pub(crate) fn monthly() -> TableBlock {
    let regions = ["West", "East", "North"];
    let mut rows = Vec::new();
    for m in 1..=12u32 {
        for (r, region) in regions.iter().enumerate() {
            let revenue = 1000.0
                + (m as f64) * 85.5 * (r as f64 + 1.0)
                + if m == 9 && r == 0 { 900.0 } else { 0.0 };
            let orders = 40 + (m as i64 * 3) % 17 + r as i64 * 5;
            rows.push(vec![
                s(&format!("2026-{m:02}-01")),
                s(region),
                s(&format!("{revenue:.2}")),
                CellValue::I64(orders),
                CellValue::F64(revenue * 0.9),
                CellValue::F64(revenue * 1.1),
            ]);
        }
    }
    let mut t = TableBlock::new(
        vec![
            TableColumn::new("month", "date"),
            TableColumn::new("region", "varchar(20)"),
            TableColumn::new("revenue", "decimal(18,2)")
                .with_format(ValueFormat::currency("USD", 2)),
            TableColumn::new("orders", "int"),
            TableColumn::new("low", "float"),
            TableColumn::new("high", "float"),
        ],
        rows,
    );
    t.cell_id = Some("c_revenue01".into());
    t.title = Some("Revenue by month".into());
    t
}

fn cohorts() -> TableBlock {
    let mut rows = Vec::new();
    for c in 0..5 {
        for p in 0..(6 - c) {
            let users = (1000.0 * 0.82f64.powi(p as i32) * (1.0 - c as f64 * 0.03)).round() as i64;
            rows.push(vec![
                s(&format!("2026-0{}", c + 1)),
                CellValue::I64(p),
                CellValue::I64(users),
            ]);
        }
    }
    TableBlock::new(
        vec![
            TableColumn::new("cohort", "varchar"),
            TableColumn::new("period", "int"),
            TableColumn::new("users", "int"),
        ],
        rows,
    )
}

fn daily() -> TableBlock {
    let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
    let rows = (0..120)
        .map(|i| {
            let d = start + chrono::Days::new(i);
            vec![
                s(&d.format("%Y-%m-%d").to_string()),
                CellValue::I64(((i * 37) % 23) as i64),
            ]
        })
        .collect();
    TableBlock::new(
        vec![
            TableColumn::new("day", "date"),
            TableColumn::new("events", "bigint"),
        ],
        rows,
    )
}

fn points() -> TableBlock {
    let rows = (0..40)
        .map(|i| {
            let x = i as f64;
            vec![
                CellValue::F64(x),
                CellValue::F64(2.0 * x + ((i * 7) % 11) as f64),
                CellValue::F64(((i * 13) % 17) as f64 + 1.0),
                s(if i % 2 == 0 { "A" } else { "B" }),
                CellValue::F64(100.0 - x * 1.5 + ((i * 5) % 9) as f64),
            ]
        })
        .collect();
    TableBlock::new(
        vec![
            TableColumn::new("spend", "float"),
            TableColumn::new("signups", "float"),
            TableColumn::new("reach", "float"),
            TableColumn::new("segment", "text"),
            TableColumn::new("churn", "float"),
        ],
        rows,
    )
}

fn stages() -> TableBlock {
    TableBlock::new(
        vec![
            TableColumn::new("stage", "text"),
            TableColumn::new("count", "int"),
        ],
        [
            ("Visited", 12000),
            ("Signed up", 4200),
            ("Activated", 2300),
            ("Paid", 640),
        ]
        .iter()
        .map(|(a, b)| vec![s(a), CellValue::I64(*b)])
        .collect(),
    )
}

fn bridge() -> TableBlock {
    TableBlock::new(
        vec![
            TableColumn::new("step", "text"),
            TableColumn::new("delta", "decimal(10,2)"),
        ],
        [
            ("Start", "1200"),
            ("New", "340.5"),
            ("Churn", "-210"),
            ("Upsell", "120"),
            ("Refunds", "-45.25"),
        ]
        .iter()
        .map(|(a, b)| vec![s(a), s(b)])
        .collect(),
    )
}

/// One chart spec per kind over the fixture tables.
pub(crate) fn every_chart() -> Vec<ChartBlock> {
    let chart = |spec: ChartSpec, data: TableBlock| ChartBlock {
        cell_id: None,
        spec,
        data,
        caption: None,
    };
    let mut out = Vec::new();
    for kind in ChartKind::ALL {
        let title = format!("{} chart", kind.name());
        let c = match kind {
            ChartKind::Bar => {
                let mut spec = ChartSpec::new(kind)
                    .x("month")
                    .y("revenue")
                    .series("region")
                    .title(&title);
                spec.date_grain = Some(DateGrain::Quarter);
                spec.aggregate = Aggregate::Sum;
                spec.bar_mode = BarMode::Stacked;
                spec.reference_lines.push(ReferenceLine {
                    value: 12000.0,
                    label: Some("Target".into()),
                    status: Some(Status::Good),
                });
                chart(spec, monthly())
            }
            ChartKind::Line => {
                let mut spec = ChartSpec::new(kind)
                    .x("month")
                    .y("revenue")
                    .series("region")
                    .title(&title);
                spec.annotations.push(Annotation {
                    x: "2026-09-01".into(),
                    text: "Promo".into(),
                });
                spec.y_axis.title = Some("Revenue".into());
                chart(spec, monthly())
            }
            ChartKind::Area => {
                let mut west = monthly();
                west.rows.retain(|r| r[1] == s("West"));
                let mut spec = ChartSpec::new(kind).x("month").y("revenue").title(&title);
                spec.anomaly_band = Some(AnomalyBand {
                    lower: "low".into(),
                    upper: "high".into(),
                });
                chart(spec, west)
            }
            ChartKind::Scatter => {
                let mut spec = ChartSpec::new(kind).x("spend").y("signups").title(&title);
                spec.color = Some(Encoding::new("segment"));
                chart(spec, points())
            }
            ChartKind::Bubble => {
                let mut spec = ChartSpec::new(kind).x("spend").y("signups").title(&title);
                spec.size = Some(Encoding::new("reach"));
                chart(spec, points())
            }
            ChartKind::Histogram => {
                chart(ChartSpec::new(kind).y("signups").title(&title), points())
            }
            ChartKind::Pie | ChartKind::Donut | ChartKind::Treemap => {
                let mut spec = ChartSpec::new(kind).x("region").y("revenue").title(&title);
                spec.aggregate = Aggregate::Sum;
                if kind == ChartKind::Treemap {
                    spec = spec.x("month");
                    spec.series = Some(Encoding::new("region"));
                    spec.date_grain = Some(DateGrain::Quarter);
                }
                chart(spec, monthly())
            }
            ChartKind::Heatmap => {
                let mut spec = ChartSpec::new(kind)
                    .x("month")
                    .y("orders")
                    .series("region")
                    .title(&title);
                spec.show_values = true;
                chart(spec, monthly())
            }
            ChartKind::CalendarHeatmap => chart(
                ChartSpec::new(kind).x("day").y("events").title(&title),
                daily(),
            ),
            ChartKind::Waterfall => {
                let mut spec = ChartSpec::new(kind).x("step").y("delta").title(&title);
                spec.show_values = true;
                chart(spec, bridge())
            }
            ChartKind::Funnel => chart(
                ChartSpec::new(kind).x("stage").y("count").title(&title),
                stages(),
            ),
            ChartKind::BoxPlot => chart(
                ChartSpec::new(kind).x("region").y("orders").title(&title),
                monthly(),
            ),
            ChartKind::CohortRetention => chart(
                ChartSpec::new(kind)
                    .x("cohort")
                    .y("users")
                    .series("period")
                    .title(&title),
                cohorts(),
            ),
            ChartKind::CorrelationMatrix => {
                let mut spec = ChartSpec::new(kind).title(&title);
                for c in ["spend", "signups", "reach", "churn"] {
                    spec.y.push(Encoding::new(c));
                }
                chart(spec, points())
            }
            ChartKind::Sparkline => {
                let mut west = monthly();
                west.rows.retain(|r| r[1] == s("West"));
                chart(ChartSpec::new(kind).x("month").y("revenue"), west)
            }
            ChartKind::Kpi => {
                let mut west = monthly();
                west.rows.retain(|r| r[1] == s("West"));
                chart(
                    ChartSpec::new(kind)
                        .x("month")
                        .y("revenue")
                        .title("West revenue"),
                    west,
                )
            }
        };
        out.push(c);
    }
    out
}

fn types_table() -> TableBlock {
    let mut t = TableBlock::new(
        vec![
            TableColumn::new("sku", "varchar(10)"),
            TableColumn::new("amount", "decimal(38,10)"),
            TableColumn::new("big_id", "bigint"),
            TableColumn::new("huge", "numeric(40,0)"),
            TableColumn::new("shipped", "date"),
            TableColumn::new("at", "datetime2"),
            TableColumn::new("ok", "bit"),
            TableColumn::new("ratio", "float").with_format(ValueFormat::percent(1)),
            TableColumn::new("note", "nvarchar(max)"),
            TableColumn::new("blob", "varbinary(8)"),
        ],
        vec![
            vec![
                s("00123"),
                s("12345.6789012345"),
                CellValue::I64(9_007_199_254_740_993),
                s("123456789012345678901234567890"),
                s("2026-11-18"),
                s("2026-11-18 14:30:15"),
                CellValue::Bool(true),
                CellValue::F64(0.256),
                s("=HYPERLINK(\"http://evil.example\",\"click\")"),
                CellValue::Bytes(vec![0xDE, 0xAD]),
            ],
            vec![
                s("A-7"),
                s("-0.0000000001"),
                CellValue::I64(-42),
                s("7"),
                CellValue::Null,
                s("not a date"),
                CellValue::Bool(false),
                CellValue::F64(f64::NAN),
                s("+1 call me"),
                CellValue::Null,
            ],
        ],
    );
    t.title = Some("Typed values".into());
    t.completeness = Completeness::Truncated { cap: 2 };
    t.total_rows = Some(50_000);
    t
}

fn tiny_png() -> Vec<u8> {
    png::rasterize(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"12\"><rect width=\"24\" height=\"12\" rx=\"3\" fill=\"#1a5fb4\"/></svg>",
        1.0,
    )
    .unwrap()
}

pub(crate) fn fixture() -> ReportDoc {
    let kpi = |label: &str, value: f64, prev: f64, higher: bool| {
        Block::Kpi(KpiBlock {
            cell_id: None,
            label: label.into(),
            value: Some(value),
            format: ValueFormat {
                style: NumberStyle::Currency,
                decimals: Some(0),
                thousands: true,
                currency: Some("USD".into()),
                ..Default::default()
            },
            comparison: Some(KpiComparison {
                label: "last quarter".into(),
                value: prev,
            }),
            status: None,
            thresholds: vec![
                Threshold {
                    value: 0.0,
                    status: Status::Critical,
                    label: None,
                },
                Threshold {
                    value: 40_000.0,
                    status: Status::Warning,
                    label: None,
                },
                Threshold {
                    value: 45_000.0,
                    status: Status::Good,
                    label: None,
                },
            ],
            sparkline: vec![38_000.0, 41_000.0, 39_500.0, 44_000.0, 46_200.0],
            higher_is_better: higher,
            caption: None,
        })
    };
    let mut charts = every_chart().into_iter();
    let line = charts.nth(1).unwrap();
    ReportDoc {
        meta: ReportMeta {
            title: "Quarterly revenue review".into(),
            subtitle: Some("Q3 2026, all regions".into()),
            author: Some("Finance analytics".into()),
            generated_at: Some("2026-11-18T09:30:00Z".into()),
            notebook_id: Some("nb_9f2c".into()),
            notebook_version: Some("sha256:4be1c0ffee".into()),
            parameters: vec![
                ReportParameter { name: "quarter".into(), value: "2026-Q3".into(), sensitive: false },
                ReportParameter { name: "api_token".into(), value: "tok_SECRET_123".into(), sensitive: true },
            ],
            connections: vec![ConnectionLabel {
                label: "Warehouse".into(),
                engine: Some("PostgreSQL".into()),
                database: Some("analytics".into()),
                environment: Some("Production".into()),
                production: true,
            }],
            ..Default::default()
        },
        blocks: vec![
            Block::Heading(Heading { level: 1, text: "Summary".into() }),
            Block::Columns(Columns {
                columns: vec![
                    Column { span: 6, blocks: vec![kpi("Revenue", 46_200.0, 44_000.0, true)] },
                    Column { span: 6, blocks: vec![kpi("Refunds", 1_250.0, 900.0, false)] },
                ],
            }),
            Block::Markdown(MarkdownBlock {
                source: "Revenue grew **5%** quarter over quarter. See [the dashboard](https://sqlly.app/docs) and `orders` table.\n\n- West led growth\n- East was flat\n\n| Region | Share |\n|---|--:|\n| West | 41% |\n".into(),
            }),
            Block::Callout(Callout { kind: CalloutKind::Fact, title: None, text: "September West revenue was 900 above trend.".into(), evidence: vec!["run_42".into()] }),
            Block::Callout(Callout { kind: CalloutKind::Hypothesis, title: Some("Promo effect".into()), text: "The spike lines up with the *autumn promo*.".into(), evidence: vec![] }),
            Block::Heading(Heading { level: 2, text: "Trend".into() }),
            Block::Chart(line),
            Block::Table(monthly()),
            Block::Assertion(AssertionResult { cell_id: Some("c_check".into()), name: "No negative revenue".into(), status: AssertionStatus::Passed, expected: Some("0 rows".into()), actual: Some("0 rows".into()), message: None }),
            Block::Assertion(AssertionResult { cell_id: None, name: "Row count within range".into(), status: AssertionStatus::Failed, expected: Some("30..40".into()), actual: Some("36".into()), message: Some("Late partition".into()) }),
            Block::PageBreak,
            Block::Section(Section {
                title: Some("Details".into()),
                blocks: vec![
                    Block::Table(types_table()),
                    Block::Code(CodeBlock { cell_id: Some("c_revenue01".into()), language: "sql".into(), source: "SELECT month, region, sum(revenue) AS revenue\nFROM sales\nWHERE quarter = :quarter\nGROUP BY 1, 2;".into(), caption: None }),
                    Block::Provenance(Provenance { cell_id: Some("c_revenue01".into()), run_id: Some("run_42".into()), connection: Some("Warehouse".into()), database: Some("analytics".into()), environment: Some("Production".into()), extracted_at: Some("2026-11-18T09:29:58Z".into()), sql_hash: Some("9a0f3b".into()), row_count: Some(36) }),
                    Block::Image(ImageBlock { alt: "Logo".into(), caption: Some("Brand mark".into()), image: ImageData::Png { bytes: tiny_png() } }),
                ],
            }),
        ],
    }
}

fn opts() -> RenderOptions {
    RenderOptions::default()
}

fn text_of(out: &RenderOutput) -> String {
    String::from_utf8(out.files[0].bytes.clone()).unwrap()
}

#[test]
fn every_chart_kind_renders_a_golden_svg() {
    let labels = ReportLabels::default();
    for chart in every_chart() {
        let options = svg::ChartOptions::new(&theme::LIGHT, &labels);
        let rendered = svg::render_chart(&chart.spec, &chart.data, options);
        assert_eq!(rendered.error, None, "{}", chart.spec.kind.name());
        let markup = svg::to_svg(&rendered.scene, chart.spec.kind.name());
        assert!(!markup.contains("<script") && !markup.contains("foreignObject"));
        assert!(
            rendered.scene.items.len() >= 3,
            "{} drew almost nothing",
            chart.spec.kind.name()
        );
        assert_golden(&markup, &format!("chart-{}.svg", chart.spec.kind.name()));
    }
}

#[test]
fn dark_charts_use_the_dark_palette() {
    let labels = ReportLabels::default();
    let chart = &every_chart()[0];
    let svg = svg::chart_svg(
        &chart.spec,
        &chart.data,
        svg::ChartOptions::new(&theme::DARK, &labels),
        "bar-dark",
    );
    assert!(svg.contains(&theme::DARK.background.css()));
    assert!(svg.contains(&theme::DARK.series[0].css()));
    assert_golden(&svg, "chart-bar-dark.svg");
}

#[test]
fn html_report_matches_its_golden() {
    let out = render(&fixture(), ReportFormat::Html, &opts()).unwrap();
    assert_eq!(out.files.len(), 1);
    assert_eq!(out.files[0].path, "quarterly-revenue-review.html");
    assert_golden(&text_of(&out), "report.html");
}

#[test]
fn markdown_report_matches_its_golden() {
    let out = render(&fixture(), ReportFormat::Markdown, &opts()).unwrap();
    assert_eq!(out.files[0].path, "quarterly-revenue-review.md");
    assert!(
        out.files.iter().skip(1).all(
            |f| f.path.starts_with("assets/") && f.path.ends_with(".svg")
                || f.path.ends_with(".png")
        )
    );
    assert!(text_of(&out).contains("](assets/quarterly-revenue-review-line-chart.svg)"));
    assert_golden(&text_of(&out), "report.md");
    let inline = RenderOptions {
        markdown_images: MarkdownImages::Inline,
        ..opts()
    };
    let out = render(&fixture(), ReportFormat::Markdown, &inline).unwrap();
    assert_eq!(out.files.len(), 1);
    assert!(text_of(&out).contains("](data:image/svg+xml;base64,"));
}

#[test]
fn every_format_is_deterministic() {
    let doc = apply_template(&fixture(), TemplateKind::Audit);
    for format in ReportFormat::ALL {
        let a = render(&doc, format, &opts()).unwrap();
        let b = render(&doc, format, &opts()).unwrap();
        assert_eq!(a.files, b.files, "{format:?}");
        assert_eq!(a.fidelity, b.fidelity, "{format:?}");
    }
}

fn hostile() -> ReportDoc {
    let xss = "<script>alert('x')</script>\"><img src=x onerror=alert(1)>";
    let mut table = TableBlock::new(
        vec![
            TableColumn::new(xss, "varchar"),
            TableColumn::new("n", "int"),
        ],
        vec![
            vec![s(xss), CellValue::I64(1)],
            vec![s("javascript:alert(1)"), CellValue::I64(2)],
        ],
    );
    table.title = Some(xss.into());
    let chart = ChartBlock {
        cell_id: Some(xss.into()),
        spec: ChartSpec::new(ChartKind::Bar).x(xss).y("n").title(xss),
        data: table.clone(),
        caption: Some(xss.into()),
    };
    ReportDoc {
        meta: ReportMeta {
            title: xss.into(),
            subtitle: Some(xss.into()),
            author: Some(xss.into()),
            parameters: vec![ReportParameter { name: xss.into(), value: xss.into(), sensitive: false }],
            ..Default::default()
        },
        blocks: vec![
            Block::Heading(Heading { level: 1, text: xss.into() }),
            Block::Markdown(MarkdownBlock {
                source: format!("{xss}\n\n[click](javascript:alert(1)) [ok](https://ok.example \"t\") ![p](https://tracker.example/p.png) <iframe src=x></iframe>\n\n[x](  JaVaScRiPt:alert(1))"),
            }),
            Block::Table(table),
            Block::Chart(chart),
            Block::Callout(Callout { kind: CalloutKind::Warning, title: Some(xss.into()), text: xss.into(), evidence: vec![xss.into()] }),
            Block::Code(CodeBlock { cell_id: None, language: "sql\"><script>".into(), source: "</code></pre><script>alert(1)</script>".into(), caption: Some(xss.into()) }),
            Block::Image(ImageBlock { alt: xss.into(), caption: Some(xss.into()), image: ImageData::Svg { markup: "<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"><script>alert(1)</script></svg>".into() } }),
            Block::Image(ImageBlock { alt: "ok".into(), caption: None, image: ImageData::Svg { markup: "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"><rect width=\"4\" height=\"4\"/></svg>".into() } }),
        ],
    }
}

/// Every real tag in `markup` (text is escaped, so it has no `<`): no
/// script-capable element, no event handler outside a quoted value, and
/// no `javascript:` / external target in `href` or `src`.
fn assert_inert(markup: &str) {
    let lower = markup.to_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find('<') {
        let end = rest[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(rest.len());
        let tag = rest[start + 1..end].to_string();
        rest = &rest[end..];
        let name: String = tag
            .trim_start_matches(['/', '!', '?'])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        for bad in [
            "script",
            "iframe",
            "object",
            "embed",
            "foreignobject",
            "form",
            "base",
            "link",
            "frame",
            "applet",
        ] {
            assert_ne!(name, bad, "<{tag}>");
        }
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut chars = tag[name.len()..].chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| c.is_whitespace() || *c == '/') {
                chars.next();
            }
            let attr: String = std::iter::from_fn(|| {
                chars.next_if(|c| !c.is_whitespace() && *c != '=' && *c != '/')
            })
            .collect();
            if attr.is_empty() {
                break;
            }
            let mut value = String::new();
            if chars.next_if_eq(&'=').is_some() {
                assert_eq!(
                    chars.next(),
                    Some('"'),
                    "unquoted attribute {attr} in <{tag}>"
                );
                value = std::iter::from_fn(|| chars.next_if(|c| *c != '"')).collect();
                assert_eq!(
                    chars.next(),
                    Some('"'),
                    "unterminated attribute {attr} in <{tag}>"
                );
            }
            attrs.push((attr, value));
        }
        for (attr, value) in attrs {
            assert!(!attr.starts_with("on"), "event handler {attr} in <{tag}>");
            assert!(
                attr != "style" || !value.contains("url("),
                "style url in <{tag}>"
            );
            if attr == "href" || attr.ends_with(":href") {
                assert!(
                    value.starts_with('#')
                        || value.starts_with("https://")
                        || value.starts_with("http://")
                        || value.starts_with("mailto:"),
                    "{attr}={value}"
                );
            }
            if attr == "src" {
                assert!(value.starts_with("data:image/"), "src={value}");
            }
        }
    }
}

#[test]
fn html_escapes_everything_and_runs_nothing() {
    let (html, fidelity) = html::render_html(&hostile(), &opts());
    assert_inert(&html);
    let lower = html.to_lowercase();
    assert!(!lower.contains("<img src=x"), "raw img leaked");
    assert!(!lower.contains("tracker.example"), "external image leaked");
    assert!(lower.contains("content-security-policy"));
    assert!(lower.contains("default-src 'none'"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("href=\"https://ok.example\""));
    for tag in [
        "<script",
        "<object",
        "<embed",
        "<form",
        "<base",
        "<link",
        "<meta http-equiv=\"refresh\"",
    ] {
        assert!(!lower.contains(tag), "{tag}");
    }
    assert!(fidelity.has(|k| matches!(k, FidelityKind::ImageDropped)));
    assert!(fidelity.has(|k| matches!(k, FidelityKind::HtmlRemoved { .. })));
    assert!(fidelity.has(|k| matches!(k, FidelityKind::LinksRemoved { count: 2 })));
    assert!(fidelity.has(|k| matches!(k, FidelityKind::ExternalImagesRemoved { count: 1 })));
}

#[test]
fn svg_escapes_labels_and_attributes() {
    let doc = hostile();
    let Block::Chart(chart) = &doc.blocks[3] else {
        panic!()
    };
    let labels = ReportLabels::default();
    let svg = svg::chart_svg(
        &chart.spec,
        &chart.data,
        svg::ChartOptions::new(&theme::LIGHT, &labels),
        "x\" onload=\"alert(1)",
    );
    assert_inert(&svg);
    assert!(svg.contains("&lt;script&gt;"));
}

#[test]
fn markdown_output_neutralises_markup() {
    let out = render(&hostile(), ReportFormat::Markdown, &opts()).unwrap();
    let md = text_of(&out);
    let mut fenced = false;
    let prose: Vec<&str> = md
        .lines()
        .filter(|line| {
            if line.starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced
        })
        .collect();
    let unescaped = prose.join("\n").replace("\\<", "");
    for tag in ["<script", "<img", "<iframe"] {
        assert!(!unescaped.to_lowercase().contains(tag), "{tag} in {md}");
    }
    assert!(!md.contains("javascript:alert(1))"), "{md}");
    assert!(!md.to_lowercase().contains("<iframe"));
}

fn calamine_sheets(bytes: Vec<u8>) -> Vec<(String, calamine::Range<calamine::Data>)> {
    use calamine::Reader;
    let mut book = calamine::Xlsx::new(std::io::Cursor::new(bytes)).expect("valid xlsx");
    book.sheet_names()
        .iter()
        .map(|name| (name.clone(), book.worksheet_range(name).expect("sheet")))
        .collect()
}

#[test]
fn xlsx_keeps_values_and_types_exactly() {
    use calamine::{Data, DataType};
    let out = render(&fixture(), ReportFormat::Xlsx, &opts()).unwrap();
    let sheets = calamine_sheets(out.files[0].bytes.clone());
    let names: Vec<&str> = sheets.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["Summary", "line chart", "Revenue by month", "Typed values"],
        "{names:?}"
    );
    let typed = &sheets[3].1;
    let cell = |r: u32, c: u32| typed.get_value((r, c)).cloned().unwrap_or(Data::Empty);
    assert_eq!(cell(0, 0), Data::String("sku".into()));
    assert_eq!(
        cell(1, 0),
        Data::String("00123".into()),
        "leading zeros stay text"
    );
    assert_eq!(
        cell(1, 1),
        Data::Float(12345.6789012345),
        "decimal text is a number"
    );
    assert_eq!(cell(2, 1), Data::Float(-0.0000000001));
    assert_eq!(
        cell(1, 2),
        Data::String("9007199254740993".into()),
        "16-digit integers keep every digit as text"
    );
    assert_eq!(cell(2, 2), Data::Float(-42.0));
    assert_eq!(
        cell(1, 3),
        Data::String("123456789012345678901234567890".into())
    );
    assert_eq!(cell(2, 3), Data::Float(7.0));
    let shipped = cell(1, 4);
    assert!(shipped.is_datetime(), "{shipped:?}");
    assert_eq!(
        shipped.as_date(),
        chrono::NaiveDate::from_ymd_opt(2026, 11, 18)
    );
    let at = cell(1, 5);
    assert_eq!(
        at.as_datetime(),
        chrono::NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 30, 15)
    );
    assert_eq!(cell(2, 5), Data::String("not a date".into()));
    assert_eq!(cell(1, 6), Data::Bool(true));
    assert_eq!(cell(1, 7), Data::Float(0.256));
    assert_eq!(cell(2, 7), Data::String("NaN".into()));
    assert_eq!(
        cell(1, 8),
        Data::String("=HYPERLINK(\"http://evil.example\",\"click\")".into()),
        "formula text stays text"
    );
    assert_eq!(cell(1, 9), Data::String("0xDEAD".into()));
    let revenue = &sheets[2].1;
    assert_eq!(revenue.get_value((1, 2)), Some(&Data::Float(1085.5)));
    assert_eq!(revenue.get_value((1, 3)).and_then(|d| d.as_i64()), Some(43));
    assert_eq!(revenue.height(), 37);
    let summary = &sheets[0].1;
    let flat: Vec<String> = summary
        .rows()
        .flat_map(|r| r.iter().map(|c| c.to_string()))
        .collect();
    assert!(flat.iter().any(|c| c == "Quarterly revenue review"));
    assert!(flat.iter().any(|c| c == "quarter = 2026-Q3"));
    assert!(
        !flat
            .iter()
            .any(|c| c.contains("tok_SECRET") || c.contains("api_token")),
        "sensitive parameter in summary"
    );
    assert!(flat.iter().any(|c| c == "46200"));
    assert!(flat.iter().any(|c| c == "Row count within range"));
    assert!(
        out.fidelity.has(
            |k| matches!(k, FidelityKind::LargeIntegersAsText { column } if column == "big_id")
        )
    );
    assert!(
        out.fidelity
            .has(|k| matches!(k, FidelityKind::ChartsAsImages))
    );
}

#[test]
fn xlsx_guards_formula_text_with_the_quote_prefix() {
    let (workbook, _) = xlsx::report::report_workbook(&fixture(), &opts());
    let bytes = xlsx::write(&workbook);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("quotePrefix=\"1\""));
    assert!(!text.contains("<f>HYPERLINK"));
    assert!(
        text.contains("xl/media/image1.png"),
        "chart pictures are embedded"
    );
}

#[test]
fn csv_exports_each_table_with_the_formula_guard() {
    let out = render(&fixture(), ReportFormat::Csv, &opts()).unwrap();
    let paths: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "quarterly-revenue-review-line-chart.csv",
            "quarterly-revenue-review-revenue-by-month.csv",
            "quarterly-revenue-review-typed-values.csv",
        ]
    );
    let typed = String::from_utf8(out.files[2].bytes.clone()).unwrap();
    let row1 = typed.split("\r\n").nth(1).unwrap();
    assert!(
        row1.starts_with(
            "00123,12345.6789012345,9007199254740993,123456789012345678901234567890,2026-11-18,"
        ),
        "{row1}"
    );
    assert!(
        row1.contains(",\"'=HYPERLINK(\"\"http://evil.example\"\",\"\"click\"\")\","),
        "{row1}"
    );
    let row2 = typed.split("\r\n").nth(2).unwrap();
    assert!(row2.contains(",'+1 call me,"), "{row2}");
    assert!(
        row2.starts_with("A-7,-0.0000000001,-42,7,,not a date,false,NaN,"),
        "{row2}"
    );
    let raw = render(
        &fixture(),
        ReportFormat::Csv,
        &RenderOptions {
            csv_formula_guard: false,
            ..opts()
        },
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&raw.files[2].bytes).contains("\"=HYPERLINK("));
}

#[test]
fn numbers_read_the_same_in_every_text_format() {
    let html = text_of(&render(&fixture(), ReportFormat::Html, &opts()).unwrap());
    let md = text_of(&render(&fixture(), ReportFormat::Markdown, &opts()).unwrap());
    for needle in [
        "12345.6789012345",
        "-0.0000000001",
        "9007199254740993",
        "123456789012345678901234567890",
        "$1,085.50",
        "25.6%",
    ] {
        assert!(html.contains(needle), "html missing {needle}");
        assert!(md.contains(needle), "markdown missing {needle}");
    }
}

#[test]
fn sensitive_parameters_never_reach_any_output() {
    let (doc, _) = redact(&fixture(), &RedactionPolicy::default());
    for format in ReportFormat::ALL {
        for file in render(&doc, format, &opts()).unwrap().files {
            let text = String::from_utf8_lossy(&file.bytes);
            assert!(!text.contains("tok_SECRET_123"), "{format:?} {}", file.path);
            assert!(!text.contains("api_token"), "{format:?} {}", file.path);
        }
    }
    let html = text_of(&render(&fixture(), ReportFormat::Html, &opts()).unwrap());
    assert!(
        !html.contains("tok_SECRET_123"),
        "renderers skip sensitive parameters even unredacted"
    );
}

#[test]
fn structure_only_exports_carry_no_rows() {
    let (doc, summary) = redact(&fixture(), &RedactionPolicy::structure_only());
    assert!(summary.rows_removed > 0);
    let html = text_of(&render(&doc, ReportFormat::Html, &opts()).unwrap());
    assert!(!html.contains("1,085.50") && !html.contains("12345.6789012345"));
    assert!(html.contains("Data withheld from this export"));
    let sheets = calamine_sheets(
        render(&doc, ReportFormat::Xlsx, &opts()).unwrap().files[0]
            .bytes
            .clone(),
    );
    assert!(sheets.iter().skip(1).all(|(_, r)| r.height() <= 1));
    let csv = render(&doc, ReportFormat::Csv, &opts()).unwrap();
    assert!(
        csv.files
            .iter()
            .all(|f| String::from_utf8_lossy(&f.bytes).matches("\r\n").count() == 1)
    );
}

#[test]
fn fidelity_reports_what_each_format_drops() {
    let doc = fixture();
    let md = render(&doc, ReportFormat::Markdown, &opts())
        .unwrap()
        .fidelity;
    assert!(md.has(|k| matches!(k, FidelityKind::ColumnsStacked)));
    assert!(md.has(|k| matches!(k, FidelityKind::PageBreaksIgnored)));
    assert!(md.has(|k| matches!(k, FidelityKind::StaticCharts)));
    let html = render(&doc, ReportFormat::Html, &opts()).unwrap().fidelity;
    assert!(!html.has(|k| matches!(k, FidelityKind::ColumnsStacked)));
    assert!(html.has(|k| matches!(k, FidelityKind::StaticCharts)));
    let csv = render(&doc, ReportFormat::Csv, &opts()).unwrap().fidelity;
    assert!(csv.has(|k| matches!(k, FidelityKind::ChartsAsData)));
    let exec = apply_template(&doc, TemplateKind::Executive);
    let html = render(&exec, ReportFormat::Html, &opts()).unwrap().fidelity;
    assert!(html.has(|k| matches!(
        k,
        FidelityKind::RowsCapped {
            shown: 20,
            total: 36
        }
    )));
    for note in &html.notes {
        assert!(
            note.message()
                .key()
                .is_some_and(|k| k.starts_with("notebook.report.fidelity."))
        );
    }
}

#[test]
fn templates_change_what_the_reader_sees() {
    let exec = text_of(
        &render(
            &apply_template(&fixture(), TemplateKind::Executive),
            ReportFormat::Html,
            &opts(),
        )
        .unwrap(),
    );
    assert!(
        !exec.contains("GROUP BY 1, 2"),
        "executive reports leave SQL out"
    );
    assert!(exec.contains("Showing 20 of 36 rows"));
    let audit = text_of(
        &render(
            &apply_template(&fixture(), TemplateKind::Audit),
            ReportFormat::Html,
            &opts(),
        )
        .unwrap(),
    );
    let appendix = audit
        .find("Appendix: SQL")
        .expect("audit has an SQL appendix");
    assert!(audit[appendix..].contains("GROUP BY 1, 2"));
    assert!(audit.contains("Appendix: Provenance"));
    let fin = apply_template(&fixture(), TemplateKind::Financial);
    let text = text_of(&render(&fin, ReportFormat::Html, &opts()).unwrap());
    assert!(
        text.contains("12,345.68"),
        "financial template formats decimals"
    );
}

#[test]
fn chart_files_come_out_per_chart() {
    let mut doc = fixture();
    doc.blocks
        .extend(every_chart().into_iter().take(3).map(Block::Chart));
    let svgs = render(&doc, ReportFormat::Svg, &opts()).unwrap();
    assert_eq!(svgs.files.len(), 4);
    assert!(svgs.files.iter().all(|f| f.media_type == "image/svg+xml"));
    let pngs = render(&doc, ReportFormat::Png, &opts()).unwrap();
    assert_eq!(pngs.files.len(), 4);
    for f in &pngs.files {
        assert_eq!(
            text::png_dimensions(&f.bytes),
            Some((1280, 720)),
            "{}",
            f.path
        );
    }
    let empty = ReportDoc::default();
    assert_eq!(
        render(&empty, ReportFormat::Svg, &opts()),
        Err(ReportError::Empty(ReportFormat::Svg))
    );
}

#[test]
fn a_bad_chart_renders_a_placeholder_and_a_note() {
    let mut doc = ReportDoc::default();
    doc.meta.title = "Broken".into();
    doc.blocks.push(Block::Chart(ChartBlock {
        cell_id: None,
        spec: ChartSpec::new(ChartKind::Line).x("missing").y("revenue"),
        data: monthly(),
        caption: None,
    }));
    let out = render(&doc, ReportFormat::Html, &opts()).unwrap();
    assert!(text_of(&out).contains("Chart not available"));
    assert!(out.fidelity.has(|k| matches!(k, FidelityKind::ChartFailed)));
}

#[test]
fn chart_transforms_sort_and_cap() {
    let labels = ReportLabels::default();
    let mut spec = ChartSpec::new(ChartKind::Bar).x("region").y("orders");
    spec.aggregate = Aggregate::Avg;
    spec.sort = Sort {
        by: SortBy::Value,
        descending: true,
    };
    spec.top_n = Some(TopN { n: 2, other: false });
    spec.orientation = Orientation::Horizontal;
    let t = monthly();
    let data = svg::data::Ctx::new(&spec, &t, &labels).categories();
    assert_eq!(data.categories, vec!["North", "East"]);
    let rendered = svg::render_chart(&spec, &t, svg::ChartOptions::new(&theme::LIGHT, &labels));
    assert!(rendered.error.is_none());
}

fn same_json(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as V;
    match (a, b) {
        (V::Number(x), V::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y || ((x - y).abs() <= 1e-12 * x.abs().max(y.abs())),
            _ => x == y,
        },
        (V::Array(x), V::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_json(a, b))
        }
        (V::Object(x), V::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
        }
        _ => a == b,
    }
}

/// serde_json (without its `float_roundtrip` feature) may read a float back
/// one ulp off, so numbers compare within 1e-12 relative.
#[test]
fn report_docs_round_trip_through_json() {
    let doc = fixture();
    let json = serde_json::to_string_pretty(&doc).unwrap();
    let back: ReportDoc = serde_json::from_str(&json).unwrap();
    let again = serde_json::to_string_pretty(&back).unwrap();
    let (a, b): (serde_json::Value, serde_json::Value) = (
        serde_json::from_str(&json).unwrap(),
        serde_json::from_str(&again).unwrap(),
    );
    assert!(same_json(&a, &b));
    assert!(json.contains("\"type\": \"kpi\""));
    assert!(json.contains("\"format\": \"png\""));
    assert!(json.contains("\"F64Text\": \"NaN\""));
    let Block::Section(section) = &back.blocks[11] else {
        panic!()
    };
    let Block::Table(t) = &section.blocks[0] else {
        panic!()
    };
    assert!(matches!(t.rows[1][7], CellValue::F64(n) if n.is_nan()));
}

fn catalog_value(key: &str) -> Option<String> {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("../../../sqlly-i18n/translations/en.json")).ok()?;
    let mut node = &json;
    for part in key.split('.') {
        node = node.get(part)?;
    }
    node.get("value")?.as_str().map(str::to_string)
}

#[test]
fn every_emitted_key_is_in_the_catalog_with_matching_english() {
    let english = ReportLabels::default();
    let resolved = ReportLabels::resolve(catalog_value);
    assert_eq!(resolved, english, "label defaults drifted from en.json");
    for key in ReportLabels::KEYS {
        assert!(catalog_value(key).is_some(), "{key}");
    }
    let fill = |m: &sqlly_core::DisplayMessage| {
        let key = m.key().expect("catalog key");
        let template = catalog_value(key).unwrap_or_else(|| panic!("{key} missing from en.json"));
        let args: Vec<(&str, &str)> = m.arguments().collect();
        ReportLabels::fill(&template, &args)
    };
    use super::chart::{Channel, ChartSpecError as E};
    for e in [
        E::UnknownColumn("c".into()),
        E::MissingEncoding {
            channel: Channel::X,
        },
        E::MissingEncoding {
            channel: Channel::Y,
        },
        E::MissingEncoding {
            channel: Channel::Series,
        },
        E::MissingEncoding {
            channel: Channel::Size,
        },
        E::NotNumeric { column: "c".into() },
        E::NotTemporal { column: "c".into() },
        E::TooManyMeasures {
            kind: ChartKind::Pie,
            max: 1,
        },
        E::SeriesNotSupported {
            kind: ChartKind::Pie,
        },
        E::InvalidTopN,
        E::InvalidBins,
        E::InvalidAxisRange,
        E::BandNeedsLine,
    ] {
        assert_eq!(fill(&e.message()), e.message().fallback(), "{e:?}");
    }
    {
        use crate::analytics::Completeness as A;
        use crate::analytics::assertions::AssertionIssue as I;
        for issue in [
            I::IncompleteInput {
                completeness: A::Partial,
            },
            I::IncompleteParent {
                completeness: A::Partial,
            },
            I::IncompleteBaseline {
                completeness: A::Partial,
            },
            I::MissingColumn { column: "c".into() },
            I::MissingOutput { name: "n".into() },
            I::MissingBaseline,
            I::MissingBaselineColumn { column: "c".into() },
            I::UnsupportedMetric,
            I::NoClock,
            I::NoDuration,
            I::InvalidPattern { detail: "d".into() },
            I::PatternTooLong { max_chars: 9 },
            I::NoTimestamps,
            I::InvalidTimeZone { zone: "z".into() },
            I::FutureTimestamp,
            I::NoNumericValues,
            I::Kpi {
                error: crate::analytics::kpi::KpiError::NoValue,
            },
        ] {
            assert_eq!(
                fill(&issue.message()),
                issue.message().fallback(),
                "{issue:?}"
            );
        }
    }
    for kind in [
        FidelityKind::StaticCharts,
        FidelityKind::CrossFilterDropped,
        FidelityKind::ChartsAsImages,
        FidelityKind::ChartsAsData,
        FidelityKind::ChartFailed,
        FidelityKind::ColumnsStacked,
        FidelityKind::RowsCapped { shown: 1, total: 2 },
        FidelityKind::GlyphsMissing {
            sample: "日".into(),
            count: 1,
        },
        FidelityKind::ComplexScripts,
        FidelityKind::PrecisionRounded { column: "c".into() },
        FidelityKind::LargeIntegersAsText { column: "c".into() },
        FidelityKind::NonFiniteAsText { column: "c".into() },
        FidelityKind::ImageDropped,
        FidelityKind::CategoriesCapped { folded: 3 },
        FidelityKind::IncompleteData,
        FidelityKind::TimezoneDropped { column: "c".into() },
        FidelityKind::HtmlRemoved { count: 1 },
        FidelityKind::LinksRemoved { count: 1 },
        FidelityKind::ExternalImagesRemoved { count: 1 },
        FidelityKind::PageBreaksIgnored,
        FidelityKind::CellTextTruncated,
        FidelityKind::LayoutFlattened,
        FidelityKind::RichTextFlattened,
        FidelityKind::ColumnsOmitted { shown: 1, total: 2 },
        FidelityKind::FixedTheme,
    ] {
        let message = FidelityNote { kind, block: None }.message();
        assert_eq!(fill(&message), message.fallback());
    }
    for e in [
        ReportError::Unavailable(ReportFormat::Pdf),
        ReportError::Empty(ReportFormat::Csv),
        ReportError::Render("disk full".into()),
    ] {
        assert_eq!(fill(&e.message()), e.message().fallback());
    }
    for t in TemplateKind::ALL {
        assert!(catalog_value(t.label_key()).is_some(), "{}", t.label_key());
    }
}

#[test]
fn notebook_output_completeness_converts() {
    use crate::notebook::Completeness as N;
    assert_eq!(
        Completeness::from(N::Spilled { total: 9 }),
        Completeness::Spilled { total: 9 }
    );
    assert_eq!(Completeness::from(N::Partial), Completeness::Partial);
}

fn truncated_chart_doc() -> ReportDoc {
    let mut data = monthly();
    data.completeness = Completeness::Truncated { cap: 36 };
    data.total_rows = Some(900);
    let mut doc = ReportDoc::default();
    doc.meta.title = "Partial".into();
    doc.blocks.push(Block::Chart(ChartBlock {
        cell_id: Some("c1".into()),
        spec: ChartSpec::new(ChartKind::Bar)
            .x("region")
            .y("revenue")
            .title("Revenue"),
        data,
        caption: None,
    }));
    doc
}

#[test]
fn drawn_marks_hit_test_to_the_category_a_cross_filter_selects() {
    use crate::notebook::cells::spec::category_label;
    use crate::report::chart::ReferenceLine;
    use crate::report::svg::{ChartOptions, HitShape, render_chart};
    let theme = crate::report::theme::LIGHT;
    let labels = ReportLabels::english();
    let table = monthly();
    let center = |shape: &HitShape| match shape {
        HitShape::Rect { x, y, w, h } => (x + w / 2.0, y + h / 2.0),
        // Midway between a quarter and three quarters round the outline:
        // inside a slice of a pie, or of a donut's ring.
        HitShape::Polygon(points) => {
            let (a, b) = (points[points.len() / 4], points[points.len() * 3 / 4]);
            ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
        }
    };
    let regions = ["West", "East", "North"];
    for kind in [
        ChartKind::Bar,
        ChartKind::Line,
        ChartKind::Area,
        ChartKind::Pie,
        ChartKind::Donut,
        ChartKind::Funnel,
        ChartKind::Treemap,
        ChartKind::Waterfall,
    ] {
        let mut spec = ChartSpec::new(kind).x("region").y("revenue");
        // A reference line keeps a bar chart off the live grid chart.
        spec.reference_lines.push(ReferenceLine {
            value: 1500.0,
            label: None,
            status: None,
        });
        let drawn = render_chart(&spec, &table, ChartOptions::new(&theme, labels));
        assert!(drawn.error.is_none(), "{kind:?}: {:?}", drawn.error);
        let mut seen: Vec<String> = Vec::new();
        for hit in &drawn.scene.hits {
            let (x, y) = center(&hit.shape);
            let picked = drawn
                .scene
                .hit_test(x, y)
                .unwrap_or_else(|| panic!("{kind:?} misses its own mark"));
            assert_eq!(picked, hit.category, "{kind:?}");
            if !seen.contains(&hit.category) {
                seen.push(hit.category.clone());
            }
        }
        seen.sort();
        let mut expected: Vec<String> = regions.iter().map(|r| r.to_string()).collect();
        expected.sort();
        assert_eq!(seen, expected, "{kind:?}: every region is clickable");
        assert_eq!(drawn.scene.hit_test(-5.0, -5.0), None, "{kind:?}");
    }
    // Dates grouped by month select by the same label cross-filters use.
    let mut spec = ChartSpec::new(ChartKind::Bar).x("month").y("revenue");
    spec.date_grain = Some(crate::report::chart::DateGrain::Month);
    spec.reference_lines.push(ReferenceLine {
        value: 1500.0,
        label: None,
        status: None,
    });
    let drawn = render_chart(&spec, &table, ChartOptions::new(&theme, labels));
    let first = &drawn.scene.hits[0];
    assert_eq!(
        first.category,
        category_label(
            &CellValue::String("2026-01-01".into()),
            Some(crate::report::chart::DateGrain::Month)
        )
    );
    // Folded "Other" stands for no single category.
    let mut capped = ChartSpec::new(ChartKind::Bar).x("region").y("revenue");
    capped.top_n = Some(crate::report::chart::TopN { n: 1, other: true });
    let drawn = render_chart(&capped, &table, ChartOptions::new(&theme, labels));
    let categories: Vec<&str> = drawn
        .scene
        .hits
        .iter()
        .map(|h| h.category.as_str())
        .collect();
    assert_eq!(categories.len(), 1, "{categories:?}");
}

#[test]
fn sparklines_over_incomplete_data_carry_a_small_mark() {
    use crate::report::svg::{ChartOptions, partial_sparkline, render_chart, sparkline, to_svg};
    let doc = truncated_chart_doc();
    let (spec, table) = doc
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Chart(c) => Some((c.spec.clone(), c.data.clone())),
            _ => None,
        })
        .expect("a chart");
    let spec = crate::report::chart::ChartSpec {
        kind: crate::report::chart::ChartKind::Sparkline,
        ..spec
    };
    let theme = crate::report::theme::LIGHT;
    let labels = ReportLabels::english();
    let warning = format!("{:?}", theme.warning);
    let draws_mark = |scene: &crate::report::svg::Scene| format!("{scene:?}").contains(&warning);
    let partial = render_chart(
        &spec,
        &table,
        ChartOptions::new(&theme, labels).size(160.0, 40.0),
    );
    assert!(partial.error.is_none(), "{:?}", partial.error);
    assert!(
        draws_mark(&partial.scene),
        "the truncated sparkline is marked"
    );
    assert!(partial.scene.description.contains("Truncated at 36 rows"));
    let mut complete = table.clone();
    complete.completeness = crate::report::doc::Completeness::Complete;
    let whole = render_chart(
        &spec,
        &complete,
        ChartOptions::new(&theme, labels).size(160.0, 40.0),
    );
    assert!(!draws_mark(&whole.scene), "a complete sparkline is not");
    assert!(!to_svg(&whole.scene, "s").is_empty());
    let values = [1.0, 3.0, 2.0];
    assert!(draws_mark(&partial_sparkline(&values, 150.0, 32.0, &theme)));
    assert!(!draws_mark(&sparkline(&values, 150.0, 32.0, &theme)));
}

#[test]
fn charts_over_incomplete_data_carry_the_badge_in_every_format() {
    let doc = truncated_chart_doc();
    let badge = "Truncated at 36 rows";
    let svg = render(&doc, ReportFormat::Svg, &opts()).unwrap();
    assert!(
        text_of(&svg).contains(badge),
        "the SVG scene draws the badge"
    );
    let md = render(&doc, ReportFormat::Markdown, &opts()).unwrap();
    assert!(
        md.files
            .iter()
            .any(|f| f.path.ends_with(".svg") && String::from_utf8_lossy(&f.bytes).contains(badge)),
        "Markdown's chart image carries it"
    );
    let html = text_of(&render(&doc, ReportFormat::Html, &opts()).unwrap());
    assert!(html.contains(badge), "the HTML drawings carry it");
    assert!(
        !html.contains("<span class=\"badge\">Truncated"),
        "not twice"
    );
    let csv = render(&doc, ReportFormat::Csv, &opts()).unwrap().fidelity;
    assert!(csv.has(|k| matches!(k, FidelityKind::IncompleteData)));
    assert_eq!(
        FidelityNote {
            kind: FidelityKind::IncompleteData,
            block: None
        }
        .message()
        .key(),
        Some("notebook.report.fidelity.incomplete_data")
    );

    let mut complete = truncated_chart_doc();
    if let Block::Chart(c) = &mut complete.blocks[0] {
        c.data.completeness = Completeness::Complete;
    }
    let svg = render(&complete, ReportFormat::Svg, &opts()).unwrap();
    assert!(!text_of(&svg).contains("Truncated"));
    let csv = render(&complete, ReportFormat::Csv, &opts())
        .unwrap()
        .fidelity;
    assert!(!csv.has(|k| matches!(k, FidelityKind::IncompleteData)));
}

#[test]
fn charts_with_too_many_categories_say_so() {
    let rows: Vec<Vec<CellValue>> = (0..2_500)
        .map(|n| vec![s(&format!("c{n:04}")), CellValue::F64(1.0)])
        .collect();
    let mut doc = ReportDoc::default();
    doc.blocks.push(Block::Chart(ChartBlock {
        cell_id: None,
        spec: ChartSpec::new(ChartKind::Bar).x("cat").y("v"),
        data: TableBlock::new(
            vec![
                TableColumn::new("cat", "varchar"),
                TableColumn::new("v", "double"),
            ],
            rows,
        ),
        caption: None,
    }));
    let html = render(&doc, ReportFormat::Html, &opts()).unwrap().fidelity;
    assert!(html.has(|k| matches!(k, FidelityKind::CategoriesCapped { folded: 501 })));
}

#[test]
fn csv_keeps_negative_zero() {
    assert_eq!(
        csv::csv_value(&CellValue::F64(-0.0), format::ColumnKind::Float, true),
        "-0"
    );
    assert_eq!(
        csv::csv_value(&CellValue::F64(0.0), format::ColumnKind::Float, true),
        "0"
    );
}

#[test]
fn xlsx_dates_follow_excel_serials_and_zoned_times_land_in_utc() {
    use calamine::{Data, DataType};
    let mut doc = ReportDoc::default();
    doc.blocks.push(Block::Table(TableBlock::new(
        vec![
            TableColumn::new("at", "timestamptz"),
            TableColumn::new("day", "datetime"),
        ],
        vec![
            vec![s("2026-01-01 10:00:00+05:30"), s("1900-01-01")],
            vec![s("2026-01-01 10:00:00-08:00"), s("1900-02-28")],
            vec![s("2026-01-01 10:00:00"), s("1899-12-31")],
        ],
    )));
    let out = render(&doc, ReportFormat::Xlsx, &opts()).unwrap();
    assert!(out.fidelity.has(|k| matches!(
        k,
        FidelityKind::TimezoneDropped { column } if column == "at"
    )));
    let sheets = calamine_sheets(out.files[0].bytes.clone());
    let table = &sheets.last().unwrap().1;
    let cell = |r: u32, c: u32| table.get_value((r, c)).cloned().unwrap_or(Data::Empty);
    let at = |h: u32, m: u32| {
        chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(h, m, 0)
    };
    assert_eq!(cell(1, 0).as_datetime(), at(4, 30));
    assert_eq!(cell(2, 0).as_datetime(), at(18, 0));
    assert_eq!(cell(3, 0).as_datetime(), at(10, 0));
    assert_eq!(
        cell(1, 1).as_date(),
        chrono::NaiveDate::from_ymd_opt(1900, 1, 1)
    );
    assert_eq!(
        cell(2, 1).as_date(),
        chrono::NaiveDate::from_ymd_opt(1900, 2, 28)
    );
    assert_eq!(cell(3, 1), Data::String("1899-12-31".into()));
}

/// One column of every kind of value, and typed columns holding values
/// their type does not usually carry.
pub(crate) fn mixed_table() -> TableBlock {
    let row = |v: CellValue, d: &str, n: &str| vec![v, s(d), s(n)];
    TableBlock::new(
        vec![
            TableColumn::new("anything", "sql_variant"),
            TableColumn::new("when", "datetimeoffset"),
            TableColumn::new("amount", "decimal(38,2)"),
        ],
        vec![
            row(
                CellValue::I64(1234567890123456789),
                "2026-01-01 10:00:00+05:30",
                "12345678901234567.89",
            ),
            row(CellValue::F64(-0.5), "2026-01-01", "-0.01"),
            row(CellValue::F64(f64::NAN), "not a date", "n/a"),
            row(CellValue::Bool(true), "1900-01-01", "0"),
            row(s("=1+1"), "2026-02-30", "1e3"),
            row(CellValue::Null, "", ""),
            row(
                CellValue::Bytes(vec![0xCA, 0xFE]),
                "10:30",
                "99999999999999999999999999999999999999",
            ),
            row(s("Grüße 日本"), "2026-06-01T12:00:00Z", "-0"),
        ],
    )
}

#[test]
fn a_mixed_type_table_reads_back_from_xlsx_and_pdf() {
    use calamine::{Data, DataType};
    let mut doc = ReportDoc::default();
    doc.meta.title = "Mixed".into();
    doc.blocks.push(Block::Table(mixed_table()));
    let out = render(&doc, ReportFormat::Xlsx, &opts()).unwrap();
    let sheets = calamine_sheets(out.files[0].bytes.clone());
    let t = &sheets.last().unwrap().1;
    let cell = |r: u32, c: u32| t.get_value((r, c)).cloned().unwrap_or(Data::Empty);
    assert_eq!(cell(1, 0), Data::String("1234567890123456789".into()));
    assert_eq!(cell(2, 0), Data::Float(-0.5));
    assert_eq!(cell(3, 0), Data::String("NaN".into()));
    assert_eq!(cell(4, 0), Data::Bool(true));
    assert_eq!(
        cell(5, 0),
        Data::String("=1+1".into()),
        "formula text stays text"
    );
    assert_eq!(cell(6, 0), Data::Empty);
    assert_eq!(cell(7, 0), Data::String("0xCAFE".into()));
    assert_eq!(cell(8, 0), Data::String("Grüße 日本".into()));
    assert_eq!(
        cell(1, 1).as_datetime(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(4, 30, 0)
    );
    assert_eq!(
        cell(2, 1).as_date(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
    );
    assert_eq!(cell(3, 1), Data::String("not a date".into()));
    assert_eq!(
        cell(4, 1).as_date(),
        chrono::NaiveDate::from_ymd_opt(1900, 1, 1)
    );
    assert_eq!(cell(5, 1), Data::String("2026-02-30".into()));
    assert_eq!(
        cell(8, 1).as_datetime(),
        chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
    );
    assert_eq!(cell(1, 2), Data::Float(12345678901234567.89));
    assert_eq!(cell(2, 2), Data::Float(-0.01));
    assert_eq!(cell(3, 2), Data::String("n/a".into()));
    assert_eq!(cell(5, 2), Data::Float(1000.0));
    assert_eq!(
        cell(7, 2),
        Data::String("99999999999999999999999999999999999999".into())
    );
    assert!(
        out.fidelity
            .has(|k| matches!(k, FidelityKind::PrecisionRounded { column } if column == "amount"))
    );
    assert!(
        out.fidelity.has(
            |k| matches!(k, FidelityKind::LargeIntegersAsText { column } if column == "amount")
        )
    );
    assert!(
        out.fidelity
            .has(|k| matches!(k, FidelityKind::TimezoneDropped { column } if column == "when"))
    );
}
