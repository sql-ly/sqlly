//! PDFs parsed back with lopdf: page count, outline, embedded fonts, text,
//! links, repeated table headers.

use lopdf::{Document, Object};

use super::render_pdf;
use crate::report::doc::*;
use crate::report::fidelity::FidelityKind;
use crate::report::tests::{every_chart, fixture, monthly};
use crate::report::{PageSize, RenderOptions, TemplateKind, apply_template};

fn load(bytes: &[u8]) -> Document {
    Document::load_mem(bytes).expect("valid PDF")
}

fn dict_get<'a>(doc: &'a Document, dict: &'a lopdf::Dictionary, key: &[u8]) -> Option<&'a Object> {
    let obj = dict.get(key).ok()?;
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

fn outline_titles(doc: &Document) -> Vec<(usize, String)> {
    let catalog = doc.catalog().expect("catalog");
    let Some(Object::Dictionary(root)) = dict_get(doc, catalog, b"Outlines") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    fn walk(doc: &Document, first: Option<&Object>, depth: usize, out: &mut Vec<(usize, String)>) {
        let mut next = first.and_then(|o| o.as_reference().ok());
        while let Some(id) = next {
            let Ok(item) = doc.get_dictionary(id) else {
                break;
            };
            let title = item
                .get(b"Title")
                .ok()
                .and_then(|t| t.as_str().ok())
                .map(decode_text)
                .unwrap_or_default();
            out.push((depth, title));
            walk(doc, item.get(b"First").ok(), depth + 1, out);
            next = item.get(b"Next").ok().and_then(|o| o.as_reference().ok());
        }
    }
    walk(doc, root.get(b"First").ok(), 0, &mut out);
    out
}

fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        bytes.iter().map(|b| char::from(*b)).collect()
    }
}

fn all_text(doc: &Document) -> String {
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    doc.extract_text(&pages).unwrap_or_default()
}

#[test]
fn the_fixture_parses_back_with_pages_outline_fonts_and_text() {
    let (bytes, fidelity) = render_pdf(&fixture(), &RenderOptions::default()).unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    let doc = load(&bytes);
    let pages = doc.get_pages();
    assert!(pages.len() >= 3, "{} pages", pages.len());

    let titles = outline_titles(&doc);
    let names: Vec<&str> = titles.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(&names[..3], &["Summary", "Trend", "Details"], "{titles:?}");
    assert_eq!(titles[1].0, 1, "Trend nests under Summary: {titles:?}");

    let mut font_files = 0;
    let mut to_unicode = 0;
    for object in doc.objects.values() {
        if let Ok(dict) = object.as_dict() {
            if dict.has(b"FontFile2") {
                font_files += 1;
            }
            if dict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Type0")
                && dict.has(b"ToUnicode")
            {
                to_unicode += 1;
            }
        }
    }
    assert_eq!(font_files, 2, "sans and mono are embedded");
    let images = doc
        .objects
        .values()
        .filter(|o| {
            o.as_stream().is_ok_and(|s| {
                s.dict.get(b"Subtype").ok().and_then(|n| n.as_name().ok()) == Some(b"Image")
            })
        })
        .count();
    assert!(images >= 1, "the fixture PNG is embedded");
    assert_eq!(to_unicode, 2);

    let text = all_text(&doc);
    for needle in [
        "Quarterly revenue review",
        "Revenue by month",
        "$1,085.50",
        "GROUP BY",
        "Page 1 of",
    ] {
        assert!(
            text.contains(needle),
            "missing {needle:?} in extracted text"
        );
    }
    assert!(!text.contains("tok_SECRET_123"));
    assert!(fidelity.has(|k| matches!(k, FidelityKind::StaticCharts)));
    assert!(
        !fidelity.has(|k| matches!(k, FidelityKind::ColumnsStacked)),
        "KPI columns lay out side by side"
    );
}

#[test]
fn rendering_is_deterministic() {
    let doc = apply_template(&fixture(), TemplateKind::Audit);
    let a = render_pdf(&doc, &RenderOptions::default()).unwrap().0;
    let b = render_pdf(&doc, &RenderOptions::default()).unwrap().0;
    assert_eq!(a, b);
}

#[test]
fn long_tables_repeat_their_header_on_every_page() {
    let mut table = monthly();
    let rows = table.rows.clone();
    for _ in 0..8 {
        table.rows.extend(rows.iter().cloned());
    }
    let total = table.rows.len();
    let doc = ReportDoc {
        meta: ReportMeta {
            title: "Long".into(),
            ..Default::default()
        },
        blocks: vec![Block::Table(table)],
    };
    let (bytes, _) = render_pdf(
        &doc,
        &RenderOptions {
            page: PageSize::Letter,
            ..Default::default()
        },
    )
    .unwrap();
    let pdf = load(&bytes);
    let pages: Vec<u32> = pdf.get_pages().keys().copied().collect();
    assert!(pages.len() >= 3, "{total} rows should span pages");
    for page in &pages {
        let text = pdf.extract_text(&[*page]).unwrap();
        assert!(
            text.contains("region") && text.contains("revenue"),
            "page {page} lacks the header"
        );
        assert!(text.contains(&format!("Page {page} of {}", pages.len())));
    }
}

#[test]
fn every_chart_kind_paints() {
    let doc = ReportDoc {
        meta: ReportMeta {
            title: "Charts".into(),
            ..Default::default()
        },
        blocks: every_chart().into_iter().map(Block::Chart).collect(),
    };
    let (bytes, fidelity) = render_pdf(&doc, &RenderOptions::default()).unwrap();
    assert!(
        !fidelity.has(|k| matches!(k, FidelityKind::ChartFailed)),
        "{fidelity:?}"
    );
    let pdf = load(&bytes);
    assert!(pdf.get_pages().len() >= 6);
}

#[test]
fn cjk_text_is_reported_as_a_fidelity_loss() {
    let doc = ReportDoc {
        meta: ReportMeta {
            title: "売上レポート".into(),
            ..Default::default()
        },
        blocks: vec![Block::Markdown(MarkdownBlock {
            source: "Привет, мир. Ελληνικά.".into(),
        })],
    };
    let (bytes, fidelity) = render_pdf(&doc, &RenderOptions::default()).unwrap();
    load(&bytes);
    assert!(
        fidelity.has(|k| matches!(k, FidelityKind::GlyphsMissing { count, .. } if *count >= 5)),
        "{fidelity:?}"
    );
    let latin = ReportDoc {
        meta: ReportMeta {
            title: "Café Ünïcödé Привет".into(),
            ..Default::default()
        },
        blocks: vec![],
    };
    let (_, fidelity) = render_pdf(&latin, &RenderOptions::default()).unwrap();
    assert!(
        !fidelity.has(|k| matches!(k, FidelityKind::GlyphsMissing { .. })),
        "{fidelity:?}"
    );
}

#[test]
fn only_safe_links_become_annotations() {
    let doc = ReportDoc {
        meta: ReportMeta { title: "Links".into(), ..Default::default() },
        blocks: vec![Block::Markdown(MarkdownBlock {
            source: "[docs](https://sqlly.app/docs) and [bad](javascript:alert(1)) and [file](file:///etc/passwd)".into(),
        })],
    };
    let (bytes, _) = render_pdf(&doc, &RenderOptions::default()).unwrap();
    let pdf = load(&bytes);
    let mut uris = Vec::new();
    for object in pdf.objects.values() {
        if let Ok(dict) = object.as_dict()
            && let Ok(action) = dict.get(b"A").and_then(|a| a.as_dict())
            && let Ok(uri) = action.get(b"URI").and_then(|u| u.as_str())
        {
            uris.push(String::from_utf8_lossy(uri).to_string());
        }
    }
    assert_eq!(uris, vec!["https://sqlly.app/docs"]);
    assert!(!String::from_utf8_lossy(&bytes).contains("JavaScript"));
}

#[test]
fn landscape_letter_and_dark_pages_have_the_right_size() {
    let options = RenderOptions {
        page: PageSize::Letter,
        landscape: true,
        theme: crate::report::ThemeMode::Dark,
        ..Default::default()
    };
    let (bytes, fidelity) = render_pdf(&fixture(), &options).unwrap();
    let pdf = load(&bytes);
    let (_, first) = pdf.get_pages().into_iter().next().unwrap();
    let page = pdf.get_dictionary(first).unwrap();
    let media = page.get(b"MediaBox").unwrap().as_array().unwrap();
    let w = media[2].as_float().unwrap();
    let h = media[3].as_float().unwrap();
    assert_eq!((w, h), (792.0, 612.0));
    assert!(fidelity.has(|k| matches!(k, FidelityKind::FixedTheme)));
}

#[test]
fn withheld_tables_print_the_notice_not_the_rows() {
    let mut table = monthly();
    table.rows.clear();
    table.data_withheld = true;
    let doc = ReportDoc {
        meta: ReportMeta {
            title: "Structure".into(),
            data_withheld: true,
            ..Default::default()
        },
        blocks: vec![Block::Table(table)],
    };
    let (bytes, _) = render_pdf(&doc, &RenderOptions::default()).unwrap();
    let text = all_text(&load(&bytes));
    assert!(text.contains("Data withheld from this export"));
    assert!(!text.contains("1,085.50"));
}

#[test]
fn a_mixed_type_table_reads_back_as_text() {
    let doc = ReportDoc {
        meta: ReportMeta {
            title: "Mixed".into(),
            ..Default::default()
        },
        blocks: vec![Block::Table(crate::report::tests::mixed_table())],
    };
    let (bytes, _) = render_pdf(&doc, &RenderOptions::default()).unwrap();
    let text = all_text(&load(&bytes));
    let squashed: String = text.split_whitespace().collect();
    for needle in [
        "1234567890123456789",
        "-0.5",
        "NaN",
        "true",
        "=1+1",
        "0xCAFE",
        "Grüße",
        "12345678901234567.89",
        "99999999999999999999999999999999999999",
        "notadate",
        "2026-02-30",
    ] {
        assert!(squashed.contains(needle), "missing {needle:?} in {text:?}");
    }
}
