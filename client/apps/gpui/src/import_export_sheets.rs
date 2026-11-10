//! Read Excel workbooks into the import wizard's string-row pipeline.

use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};

use crate::import_export::IMPORT_FILE_BYTE_CAP;

const MAX_SHEET_ROWS: usize = 1_000_000;

pub struct WorkbookInfo {
    pub sheet_names: Vec<String>,
}

pub fn open_workbook_sheets(path: &Path) -> Result<WorkbookInfo, String> {
    let workbook = open_workbook_auto(path).map_err(|error| {
        crate::localization::format(
            "results.import.workbook_open_failed",
            &[("error", &error.to_string())],
        )
    })?;
    Ok(WorkbookInfo {
        sheet_names: workbook
            .sheet_names()
            .into_iter()
            .map(|name| name.to_string())
            .collect(),
    })
}

pub fn read_sheet_rows(path: &Path, sheet: &str) -> Result<SheetReadResult, String> {
    read_sheet_rows_inner(path, sheet)
}

#[derive(Debug)]
pub struct SheetReadResult {
    pub rows: Vec<Vec<String>>,
    pub error_cells: usize,
}

pub(crate) fn read_sheet_rows_inner(path: &Path, sheet: &str) -> Result<SheetReadResult, String> {
    let mut workbook = open_workbook_auto(path).map_err(|error| {
        crate::localization::format(
            "results.import.workbook_open_failed",
            &[("error", &error.to_string())],
        )
    })?;
    let range = workbook.worksheet_range(sheet).map_err(|error| {
        crate::localization::format(
            "results.import.sheet_read_failed",
            &[("sheet_name", sheet), ("error", &error.to_string())],
        )
    })?;
    let mut rows = Vec::new();
    let mut error_cells = 0usize;
    for row in range.rows() {
        if rows.len() >= MAX_SHEET_ROWS {
            return Err(crate::localization::text("results.import.sheet_row_limit"));
        }
        let mut cells = row.iter().map(cell_to_string).collect::<Vec<_>>();
        while cells.last().is_some_and(String::is_empty) {
            cells.pop();
        }
        if cells.iter().all(String::is_empty) {
            continue;
        }
        for cell in row {
            if matches!(cell, Data::Error(_)) {
                error_cells += 1;
            }
        }
        rows.push(cells);
    }
    let _ = IMPORT_FILE_BYTE_CAP;
    if error_cells > 0 {
        tracing::warn!("{error_cells} Excel error cells read as NULL");
    }
    Ok(SheetReadResult { rows, error_cells })
}

pub fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.trim_end_matches('\r').to_string(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => {
            if value.fract() == 0.0 && value.abs() < 1e15 {
                format!("{}", *value as i64)
            } else {
                format!("{value}")
            }
        }
        Data::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Data::DateTime(value) => value
            .as_datetime()
            .map(|timestamp| {
                if timestamp.time() == chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
                    timestamp.format("%Y-%m-%d").to_string()
                } else {
                    timestamp.format("%Y-%m-%dT%H:%M:%S").to_string()
                }
            })
            .unwrap_or_default(),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::Error(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_export::{CsvType, infer_csv_types};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/import")
            .join(name)
    }

    #[test]
    fn cell_to_string_matrix() {
        use calamine::{CellErrorType, ExcelDateTime, ExcelDateTimeType};
        assert_eq!(cell_to_string(&Data::Empty), "");
        assert_eq!(cell_to_string(&Data::String("x\r".into())), "x");
        assert_eq!(cell_to_string(&Data::Int(42)), "42");
        assert_eq!(cell_to_string(&Data::Float(42.0)), "42");
        assert_eq!(cell_to_string(&Data::Float(3.5)), "3.5");
        assert_eq!(cell_to_string(&Data::Bool(true)), "true");
        let midnight = ExcelDateTime::new(45_000.0, ExcelDateTimeType::DateTime, false);
        assert_eq!(cell_to_string(&Data::DateTime(midnight)), "2023-03-15");
        let with_time = ExcelDateTime::new(45_000.5, ExcelDateTimeType::DateTime, false);
        assert!(cell_to_string(&Data::DateTime(with_time)).contains('T'));
        assert_eq!(
            cell_to_string(&Data::DateTimeIso("2024-01-01T00:00:00".into())),
            "2024-01-01T00:00:00"
        );
        assert_eq!(cell_to_string(&Data::Error(CellErrorType::Div0)), "");
    }

    #[test]
    fn two_sheet_workbook_lists_sheet_names() {
        let path = fixture("two_sheets.xlsx");
        let info = open_workbook_sheets(&path).expect("open workbook");
        assert_eq!(info.sheet_names, vec!["Orders", "Empty"]);
    }

    #[test]
    fn orders_sheet_rows_infer_expected_types() {
        let path = fixture("two_sheets.xlsx");
        let rows = read_sheet_rows(&path, "Orders").expect("read orders");
        assert_eq!(rows.rows.len(), 4);
        let types = infer_csv_types(&rows.rows[1..], rows.rows[0].len(), "");
        assert_eq!(
            types,
            vec![
                CsvType::Integer,
                CsvType::Decimal,
                CsvType::Date,
                CsvType::Boolean,
                CsvType::Text,
            ]
        );
    }

    #[test]
    fn empty_sheet_yields_no_rows() {
        let path = fixture("two_sheets.xlsx");
        let rows = read_sheet_rows(&path, "Empty").expect("read empty");
        assert!(rows.rows.is_empty());
    }

    #[test]
    fn legacy_xls_reads_rows() {
        let path = fixture("legacy.xls");
        let rows = read_sheet_rows(&path, "Sheet1").expect("read xls");
        assert_eq!(rows.rows.len(), 2);
    }

    #[test]
    fn unknown_sheet_name_errors() {
        let path = fixture("two_sheets.xlsx");
        let error = read_sheet_rows(&path, "Missing").unwrap_err();
        assert!(error.contains("Missing"), "{error}");
    }
}
