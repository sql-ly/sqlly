//! The plain single-sheet `.xlsx` export (port of Swift's
//! `ResultSetExporter.excelXLSX`): one worksheet registered as a native
//! Excel Table (ListObject) with banded rows and an auto-filter header,
//! zipped with the built-in STORE writer.

use sqlly_core::{CellValue, ResultColumn};

use super::{excel_column_label, sanitize_excel_name, unique_column_names};
use crate::report::format::{ColumnKind, numeric_literal};
use crate::report::text::xml_escape;

use super::zip::zip_archive;

/// [`excel_xlsx_with`] classifying numeric columns by their declared type.
pub fn excel_xlsx(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    sheet_name: &str,
    table_name: &str,
) -> Vec<u8> {
    excel_xlsx_with(columns, rows, sheet_name, table_name, |c| {
        c.is_some_and(|c| ColumnKind::infer(&c.data_type).is_numeric())
    })
}

/// Builds a real `.xlsx` workbook as raw bytes. The single worksheet holds
/// the result set and is registered as a native Excel Table spanning the
/// full data range. `is_numeric` says which declared types are numbers, so
/// decimal text from the driver lands as a number cell.
pub fn excel_xlsx_with(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    sheet_name: &str,
    table_name: &str,
    is_numeric: impl Fn(Option<&ResultColumn>) -> bool,
) -> Vec<u8> {
    let column_names = unique_column_names(columns);
    let has_table = !column_names.is_empty();
    let row_count = rows.len();
    let last_col_label = excel_column_label(column_names.len().max(1));
    let table_ref = format!("A1:{}{}", last_col_label, row_count + 1);
    let safe_sheet = if sheet_name.trim().is_empty() {
        "Results"
    } else {
        sheet_name
    };
    let numeric: Vec<bool> = (0..column_names.len())
        .map(|i| is_numeric(columns.get(i)))
        .collect();

    let mut parts: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".to_string(),
            xml_bytes(&content_types_xml(has_table)),
        ),
        ("_rels/.rels".to_string(), xml_bytes(&root_rels_xml())),
        (
            "xl/workbook.xml".to_string(),
            xml_bytes(&workbook_xml(safe_sheet)),
        ),
        (
            "xl/_rels/workbook.xml.rels".to_string(),
            xml_bytes(&workbook_rels_xml()),
        ),
        (
            "xl/worksheets/sheet1.xml".to_string(),
            xml_bytes(&worksheet_xml(
                &numeric,
                &column_names,
                rows,
                &table_ref,
                has_table,
            )),
        ),
    ];
    if has_table {
        parts.push((
            "xl/worksheets/_rels/sheet1.xml.rels".to_string(),
            xml_bytes(&worksheet_rels_xml()),
        ));
        parts.push((
            "xl/tables/table1.xml".to_string(),
            xml_bytes(&table_xml(&column_names, &table_ref, table_name)),
        ));
    }
    zip_archive(&parts)
}

fn xml_bytes(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

fn content_types_xml(has_table: bool) -> String {
    let mut xml = XML_DECLARATION.to_string();
    xml.push_str("<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">");
    xml.push_str("<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>");
    xml.push_str("<Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    xml.push_str("<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>");
    xml.push_str("<Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>");
    if has_table {
        xml.push_str("<Override PartName=\"/xl/tables/table1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml\"/>");
    }
    xml.push_str("</Types>");
    xml
}

fn root_rels_xml() -> String {
    format!(
        "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"
    )
}

fn workbook_xml(sheet_name: &str) -> String {
    format!(
        "{XML_DECLARATION}<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"{}\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>",
        xml_escape(sheet_name)
    )
}

fn workbook_rels_xml() -> String {
    format!(
        "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>"
    )
}

fn worksheet_rels_xml() -> String {
    format!(
        "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/table\" Target=\"../tables/table1.xml\"/></Relationships>"
    )
}

fn worksheet_xml(
    numeric: &[bool],
    column_names: &[String],
    rows: &[Vec<CellValue>],
    table_ref: &str,
    has_table: bool,
) -> String {
    let mut xml = XML_DECLARATION.to_string();
    xml.push_str("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">");
    let dim = if has_table { table_ref } else { "A1" };
    xml.push_str(&format!("<dimension ref=\"{}\"/>", dim));
    xml.push_str("<sheetData>");
    if has_table {
        xml.push_str("<row r=\"1\">");
        for (index, name) in column_names.iter().enumerate() {
            xml.push_str(&inline_string_cell(
                &format!("{}1", excel_column_label(index + 1)),
                name,
            ));
        }
        xml.push_str("</row>");
        for (row_index, row) in rows.iter().enumerate() {
            let row_number = row_index + 2;
            xml.push_str(&format!("<row r=\"{}\">", row_number));
            for (col_index, is_numeric) in numeric.iter().enumerate() {
                let value = row.get(col_index).unwrap_or(&CellValue::Null);
                let cell_ref = format!("{}{}", excel_column_label(col_index + 1), row_number);
                xml.push_str(&cell_xml(&cell_ref, value, *is_numeric));
            }
            xml.push_str("</row>");
        }
    }
    xml.push_str("</sheetData>");
    if has_table {
        xml.push_str("<tableParts count=\"1\"><tablePart r:id=\"rId1\"/></tableParts>");
    }
    xml.push_str("</worksheet>");
    xml
}

/// One `<c>` element. `numeric_column` says the column's declared type is
/// numeric, which lets a string cell (decimal/money arrive as text) be written
/// as a number instead of an inline string.
fn cell_xml(ref_id: &str, value: &CellValue, numeric_column: bool) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(flag) => {
            format!(
                "<c r=\"{}\" t=\"b\"><v>{}</v></c>",
                ref_id,
                if *flag { 1 } else { 0 }
            )
        }
        CellValue::I64(number) => {
            format!("<c r=\"{}\"><v>{}</v></c>", ref_id, number)
        }
        CellValue::F64(number) => {
            if number.is_finite() {
                format!("<c r=\"{}\"><v>{}</v></c>", ref_id, number)
            } else {
                String::new()
            }
        }
        CellValue::String(text) => match numeric_column.then(|| numeric_literal(text)).flatten() {
            Some(number) => format!("<c r=\"{}\"><v>{}</v></c>", ref_id, number),
            None => inline_string_cell(ref_id, text),
        },
        CellValue::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{:02X}", b)).collect();
            inline_string_cell(ref_id, &format!("0x{}", hex))
        }
    }
}

fn inline_string_cell(ref_id: &str, text: &str) -> String {
    format!(
        "<c r=\"{}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
        ref_id,
        xml_escape(text)
    )
}

fn table_xml(column_names: &[String], table_ref: &str, table_name: &str) -> String {
    let display = sanitize_excel_name(table_name);
    let mut xml = XML_DECLARATION.to_string();
    xml.push_str(&format!(
        "<table xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" id=\"1\" name=\"{}\" displayName=\"{}\" ref=\"{}\" totalsRowShown=\"0\">",
        display, display, table_ref
    ));
    xml.push_str(&format!("<autoFilter ref=\"{}\"/>", table_ref));
    xml.push_str(&format!("<tableColumns count=\"{}\">", column_names.len()));
    for (index, name) in column_names.iter().enumerate() {
        xml.push_str(&format!(
            "<tableColumn id=\"{}\" name=\"{}\"/>",
            index + 1,
            xml_escape(name)
        ));
    }
    xml.push_str("</tableColumns>");
    xml.push_str("<tableStyleInfo name=\"TableStyleMedium2\" showFirstColumn=\"0\" showLastColumn=\"0\" showRowStripes=\"1\" showColumnStripes=\"0\"/>");
    xml.push_str("</table>");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn row(values: &[CellValue]) -> Vec<CellValue> {
        values.to_vec()
    }

    fn numeric(c: &[ResultColumn]) -> Vec<bool> {
        c.iter()
            .map(|c| ColumnKind::infer(&c.data_type).is_numeric())
            .collect()
    }

    #[test]
    fn xlsx_produces_valid_zip() {
        let c = vec![col("id", "int"), col("name", "varchar")];
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        // ZIP local file header signature
        assert!(bytes.starts_with(&0x0403_4b50u32.to_le_bytes()));
        // End of central directory signature
        let eocd = &bytes[bytes.len() - 22..];
        assert_eq!(&eocd[0..4], &0x0605_4b50u32.to_le_bytes());
    }

    #[test]
    fn xlsx_contains_worksheet_and_table() {
        let c = vec![col("id", "int"), col("name", "varchar")];
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("xl/worksheets/sheet1.xml"));
        assert!(text.contains("xl/tables/table1.xml"));
        assert!(text.contains("TableStyleMedium2"));
    }

    #[test]
    fn xlsx_contains_cell_values() {
        let c = vec![col("id", "int"), col("name", "varchar")];
        let r = row(&[CellValue::I64(42), CellValue::String("Test".into())]);
        let bytes = excel_xlsx(&c, &[r], "Sheet1", "MyTable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("<v>42</v>"));
        assert!(text.contains("Test"));
        assert!(text.contains("Sheet1"));
    }

    #[test]
    fn xlsx_handles_null_as_omitted_cell() {
        let c = vec![col("id", "int"), col("name", "varchar")];
        let r = row(&[CellValue::I64(1), CellValue::Null]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("<v>1</v>"));
    }

    #[test]
    fn xlsx_handles_bool_and_double() {
        let c = vec![col("flag", "bit"), col("price", "float")];
        let r = row(&[CellValue::Bool(true), CellValue::F64(1.5)]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("t=\"b\"><v>1</v>"));
        assert!(text.contains("<v>1.5</v>"));
    }

    #[test]
    fn decimal_text_in_a_numeric_column_becomes_a_number_cell() {
        // The TDS driver renders decimal/numeric/money to a string to keep
        // full precision; those must still land in Excel as numbers.
        let c = vec![col("total", "money"), col("qty", "decimal(38,10)")];
        let r = row(&[
            CellValue::String("1234.56".into()),
            CellValue::String("-0.0000000001".into()),
        ]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("<v>1234.56</v>"), "{text}");
        // Digits survive verbatim — no f64 round-trip.
        assert!(text.contains("<v>-0.0000000001</v>"), "{text}");
        // Only the header row is text.
        assert_eq!(text.matches("inlineStr").count(), 2, "{text}");
    }

    #[test]
    fn numeric_looking_text_in_a_text_column_stays_text() {
        // Leading zeros, product codes, and phone numbers must not be
        // silently converted to numbers.
        let c = vec![col("sku", "varchar"), col("zip", "nvarchar")];
        let r = row(&[
            CellValue::String("00123".into()),
            CellValue::String("90210".into()),
        ]);
        let bytes = excel_xlsx(&c, &[r], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("00123"));
        assert!(!text.contains("<v>00123</v>"), "{text}");
    }

    #[test]
    fn formula_looking_text_is_a_string_cell_never_a_formula() {
        // XLSX cells are typed: text that starts with `=`, `+`, `-`, `@`
        // lands as an inline string (no `<f>` element), so a workbook never
        // needs the apostrophe guard the delimited exports use.
        let c = vec![col("note", "varchar")];
        for text in [
            "=1+1",
            "=HYPERLINK(\"http://x\")",
            "+cmd",
            "-cmd",
            "@SUM(1)",
        ] {
            let names = vec!["note".to_string()];
            let xml = worksheet_xml(
                &numeric(&c),
                &names,
                &[row(&[CellValue::String(text.into())])],
                "A1:A2",
                true,
            );
            assert!(!xml.contains("<f>"), "{text:?} must not become a formula");
            assert!(!xml.contains("<f "), "{text:?} must not become a formula");
            assert!(
                xml.contains(&format!(
                    "t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t>",
                    xml_escape(text)
                )),
                "{text:?} should be an inline string cell: {xml}"
            );
        }
    }

    #[test]
    fn unparseable_text_in_a_numeric_column_stays_text() {
        let c = vec![col("total", "money")];
        for value in ["$1,234.56", "1 234", "N/A", "", "NaN", "1.2.3"] {
            let r = row(&[CellValue::String(value.into())]);
            let bytes = excel_xlsx(&c, &[r], "Results", "Results");
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("inlineStr"),
                "{value:?} should stay an inline string"
            );
        }
    }

    #[test]
    fn xlsx_empty_rows_still_has_header() {
        let c = vec![col("id", "int")];
        let bytes = excel_xlsx(&c, &[], "Results", "Results");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("id"));
        // Table ref should span at least row 1
        assert!(text.contains("A1:A1"));
    }

    /// A Redis `string` column is text to the classifier, so `00014` lands
    /// in the worksheet as an inline string, not a numeric cell that Excel
    /// would show as 14.
    #[test]
    fn redis_text_values_stay_inline_strings_in_the_worksheet() {
        let c = vec![col("result", "string")];
        let r = row(&[CellValue::String("00014".into())]);
        let xml = worksheet_xml(&numeric(&c), &["result".to_string()], &[r], "A1:A2", true);
        assert!(
            xml.contains(
                "<c r=\"A2\" t=\"inlineStr\"><is><t xml:space=\"preserve\">00014</t></is></c>"
            ),
            "{xml}"
        );
        // The same digits in a numeric column are a number, so the rule is
        // the column's type, not the digits.
        let n = vec![col("result", "int")];
        let r = row(&[CellValue::String("00014".into())]);
        let xml = worksheet_xml(&numeric(&n), &["result".to_string()], &[r], "A1:A2", true);
        assert!(xml.contains("<v>00014</v>"), "{xml}");
    }
}
