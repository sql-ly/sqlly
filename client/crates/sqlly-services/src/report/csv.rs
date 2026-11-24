//! CSV per table (RFC 4180: quoted where needed, CRLF rows). Values keep
//! their exact digits; text cells a spreadsheet would run as a formula get
//! the shared apostrophe guard unless the caller turns it off.

use sqlly_core::CellValue;
use sqlly_core::formula_guard::guard_text;

use super::doc::{Block, ReportDoc, TableBlock, walk};
use super::fidelity::{FidelityKind, FidelityReport};
use super::format::{ColumnKind, float_literal, hex_bytes, numeric_literal};
use super::text::Anchors;
use super::{RenderOptions, RenderedFile, ReportFormat};

fn quote(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) || field.starts_with(' ') || field.ends_with(' ') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// One cell as CSV text (before quoting).
pub fn csv_value(value: &CellValue, kind: ColumnKind, guard: bool) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => if *b { "true" } else { "false" }.into(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(n) if *n == 0.0 && n.is_sign_negative() => "-0".into(),
        CellValue::F64(n) if n.is_finite() => float_literal(*n),
        CellValue::F64(n) => if n.is_nan() {
            "NaN"
        } else if *n > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into(),
        CellValue::Bytes(b) => hex_bytes(b, usize::MAX),
        CellValue::String(s) => {
            if kind.is_numeric()
                && let Some(literal) = numeric_literal(s)
            {
                return literal.to_string();
            }
            if guard {
                guard_text(s).into_owned()
            } else {
                s.clone()
            }
        }
    }
}

/// The table as CSV text: a header row, then every row the table holds.
pub fn table_csv(table: &TableBlock, guard: bool) -> String {
    let mut out = String::new();
    let header: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            quote(&if guard {
                guard_text(&c.name).into_owned()
            } else {
                c.name.clone()
            })
        })
        .collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    let kinds: Vec<ColumnKind> = table.columns.iter().map(|c| c.kind()).collect();
    for row in &table.rows {
        let cells: Vec<String> = kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                quote(&csv_value(
                    row.get(i).unwrap_or(&CellValue::Null),
                    *kind,
                    guard,
                ))
            })
            .collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

pub fn render_csv(
    doc: &ReportDoc,
    options: &RenderOptions,
    stem: &str,
) -> (Vec<RenderedFile>, FidelityReport) {
    let mut fidelity = FidelityReport::new(ReportFormat::Csv);
    let mut tables: Vec<(&TableBlock, Option<String>)> = Vec::new();
    let mut charts = 0usize;
    let mut other = false;
    walk(&doc.blocks, &mut |b| match b {
        Block::Table(t) => tables.push((t, t.title.clone())),
        Block::Chart(c) => {
            charts += 1;
            tables.push((
                &c.data,
                c.spec.title.clone().or_else(|| c.data.title.clone()),
            ));
        }
        Block::Section(_) | Block::Columns(_) | Block::PageBreak | Block::Heading(_) => {}
        _ => other = true,
    });
    if charts > 0 {
        fidelity.note_once(FidelityKind::ChartsAsData);
    }
    for (table, title) in &tables {
        fidelity.note_completeness(table, title.as_deref());
    }
    if other {
        fidelity.note_once(FidelityKind::LayoutFlattened);
    }
    let mut names = Anchors::default();
    let files = tables
        .into_iter()
        .enumerate()
        .map(|(i, (t, title))| {
            let label = title
                .or_else(|| t.cell_id.clone())
                .unwrap_or_else(|| format!("table {}", i + 1));
            RenderedFile {
                path: format!("{stem}-{}.csv", names.next(&label)),
                media_type: ReportFormat::Csv.media_type(),
                bytes: table_csv(t, options.csv_formula_guard).into_bytes(),
            }
        })
        .collect();
    (files, fidelity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::doc::TableColumn;

    fn table(values: Vec<CellValue>) -> TableBlock {
        TableBlock::new(
            vec![
                TableColumn::new("note", "varchar"),
                TableColumn::new("amount", "decimal(38,10)"),
            ],
            vec![values],
        )
    }

    #[test]
    fn formula_triggers_are_guarded() {
        for (text, expected) in [
            ("=1+1", "'=1+1"),
            ("+cmd|' /C calc'!A0", "'+cmd|' /C calc'!A0"),
            ("-2+3", "'-2+3"),
            ("@SUM(A1)", "'@SUM(A1)"),
            ("\t=1", "'\t=1"),
            (
                "=HYPERLINK(\"http://x\",\"y\")",
                "\"'=HYPERLINK(\"\"http://x\"\",\"\"y\"\")\"",
            ),
            ("-12.5", "-12.5"),
            ("plain", "plain"),
        ] {
            let csv = table_csv(
                &table(vec![CellValue::String(text.into()), CellValue::Null]),
                true,
            );
            let line = csv.split("\r\n").nth(1).unwrap();
            assert_eq!(line, format!("{expected},"), "{text:?}");
        }
    }

    #[test]
    fn the_guard_can_be_turned_off() {
        let csv = table_csv(
            &table(vec![CellValue::String("=1+1".into()), CellValue::Null]),
            false,
        );
        assert!(csv.contains("\r\n=1+1,"));
    }

    #[test]
    fn numbers_keep_their_digits() {
        let csv = table_csv(
            &table(vec![
                CellValue::String("x".into()),
                CellValue::String("-12345678901234567890.0000000001".into()),
            ]),
            true,
        );
        assert!(
            csv.ends_with("x,-12345678901234567890.0000000001\r\n"),
            "{csv}"
        );
        assert_eq!(
            csv_value(&CellValue::I64(i64::MIN), ColumnKind::Integer, true),
            "-9223372036854775808"
        );
        assert_eq!(
            csv_value(&CellValue::F64(0.1), ColumnKind::Float, true),
            "0.1"
        );
        assert_eq!(
            csv_value(&CellValue::String("00123".into()), ColumnKind::Text, true),
            "00123"
        );
    }

    #[test]
    fn quoting_follows_rfc_4180() {
        assert_eq!(quote("a,b"), "\"a,b\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(quote("plain"), "plain");
    }
}
