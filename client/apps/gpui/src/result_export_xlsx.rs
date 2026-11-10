//! XLSX (Office Open XML) workbook export — dependency-free.
//!
//! Port of `ResultSetExporter.excelXLSX` from Swift. Produces a real
//! `.xlsx` file with a single worksheet registered as a native Excel Table
//! (ListObject) with banded rows and an auto-filter header. The archive
//! is produced with a built-in STORE-method ZIP writer (no compression,
//! no external dependencies).
//!
//! All functions are pure and take `&[ResultColumn]` + `&[Vec<CellValue>]`
//! so they can be unit-tested without any GPUI types.

use sqlly_core::{CellValue, ResultColumn};

use crate::result_cell_format::{column_is_numeric, numeric_literal};
use crate::result_export::{excel_column_label, unique_column_names, xml_escape};

/// Builds a real `.xlsx` workbook as raw bytes. The single worksheet holds
/// the result set and is registered as a native Excel Table spanning the
/// full data range, so the file opens already formatted with banded rows
/// and an auto-filter header.
pub fn excel_xlsx(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    sheet_name: &str,
    table_name: &str,
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
                columns,
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
    columns: &[ResultColumn],
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
    let numeric: Vec<bool> = (0..column_names.len())
        .map(|i| column_is_numeric(columns.get(i)))
        .collect();
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

/// Excel defined-names (and a table's displayName) must begin with a letter
/// or underscore and contain only letters, digits, underscores, or periods.
pub(crate) fn sanitize_excel_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut result = String::new();
    for ch in trimmed.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        return "Results".to_string();
    }
    if let Some(first) = result.chars().next()
        && !first.is_alphabetic()
        && first != '_'
    {
        result.insert(0, '_');
    }
    result
}

// ---------------------------------------------------------------------------
// Minimal ZIP (STORE) writer — no compression, no external dependencies.
// ---------------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut j = 0;
        while j < 8 {
            if value & 1 == 1 {
                value = 0xEDB8_8320 ^ (value >> 1);
            } else {
                value >>= 1;
            }
            j += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

struct ZipEntry {
    name_bytes: Vec<u8>,
    data: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
}

/// DOS date 1980-01-01, time 00:00:00 — a fixed, valid timestamp.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0x0000;

pub(crate) fn zip_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut metas: Vec<ZipEntry> = entries
        .iter()
        .map(|(name, data)| ZipEntry {
            name_bytes: name.as_bytes().to_vec(),
            crc: crc32(data),
            size: data.len() as u32,
            data: data.clone(),
            offset: 0,
        })
        .collect();

    let mut local: Vec<u8> = Vec::new();
    for meta in metas.iter_mut() {
        meta.offset = local.len() as u32;
        let name_bytes = &meta.name_bytes;
        let crc = meta.crc;
        let size = meta.size;

        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // STORE
        local.extend_from_slice(&DOS_TIME.to_le_bytes());
        local.extend_from_slice(&DOS_DATE.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_bytes);
        local.extend_from_slice(&meta.data);
    }

    let mut central: Vec<u8> = Vec::new();
    for meta in metas.iter() {
        let name_bytes = &meta.name_bytes;
        let crc = meta.crc;
        let size = meta.size;
        let offset = meta.offset;

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // STORE
        central.extend_from_slice(&DOS_TIME.to_le_bytes());
        central.extend_from_slice(&DOS_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }

    let mut archive: Vec<u8> = Vec::with_capacity(local.len() + central.len() + 22);
    let central_offset = local.len() as u32;
    archive.extend_from_slice(&local);
    archive.extend_from_slice(&central);
    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&(metas.len() as u16).to_le_bytes());
    archive.extend_from_slice(&(metas.len() as u16).to_le_bytes());
    archive.extend_from_slice(&(central.len() as u32).to_le_bytes());
    archive.extend_from_slice(&central_offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive
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
        // Null cells are omitted (empty string), so the row should still
        // contain the integer cell but not an inlineStr for the null.
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
                &c,
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

    #[test]
    fn sanitize_excel_name_basic() {
        assert_eq!(sanitize_excel_name("Results"), "Results");
        assert_eq!(sanitize_excel_name("My Table!"), "My_Table_");
        assert_eq!(sanitize_excel_name(""), "Results");
        assert_eq!(sanitize_excel_name("123abc"), "_123abc");
    }

    #[test]
    fn crc32_known_values() {
        // CRC32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // CRC32 of empty data
        assert_eq!(crc32(b""), 0x0000_0000);
    }

    #[test]
    fn zip_archive_basic() {
        let entries = vec![
            ("a.txt".to_string(), b"hello".to_vec()),
            ("b.txt".to_string(), b"world".to_vec()),
        ];
        let archive = zip_archive(&entries);
        // Starts with local file header
        assert!(archive.starts_with(&0x0403_4b50u32.to_le_bytes()));
        // Ends with end of central directory record
        let eocd = &archive[archive.len() - 22..];
        assert_eq!(&eocd[0..4], &0x0605_4b50u32.to_le_bytes());
        // Entry count in EOCD
        let count = u16::from_le_bytes([eocd[10], eocd[11]]);
        assert_eq!(count, 2);
    }

    /// A Redis `string` column is text to the classifier, so `00014` lands
    /// in the worksheet as an inline string, not a numeric cell that Excel
    /// would show as 14.
    #[test]
    fn redis_text_values_stay_inline_strings_in_the_worksheet() {
        let c = vec![col("result", "string")];
        let r = row(&[CellValue::String("00014".into())]);
        let xml = worksheet_xml(&c, &["result".to_string()], &[r], "A1:A2", true);
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
        let xml = worksheet_xml(&n, &["result".to_string()], &[r], "A1:A2", true);
        assert!(xml.contains("<v>00014</v>"), "{xml}");
    }

    #[test]
    fn zip_archive_contains_file_data() {
        let entries = vec![("test.txt".to_string(), b"hello world".to_vec())];
        let archive = zip_archive(&entries);
        let text = String::from_utf8_lossy(&archive);
        assert!(text.contains("hello world"));
        assert!(text.contains("test.txt"));
    }
}
