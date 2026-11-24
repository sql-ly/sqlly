//! XLSX (Office Open XML) workbook export — dependency-free.
//!
//! The single-sheet writer (one worksheet registered as a native Excel
//! Table with banded rows and an auto-filter header, zipped with a STORE
//! writer) lives in `sqlly_services::report::xlsx`; this keeps the app's
//! entry point and feeds it the app's numeric-column classifier.

use sqlly_core::{CellValue, ResultColumn};

use crate::result_cell_format::column_is_numeric;

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
    sqlly_services::report::xlsx::excel_xlsx_with(
        columns,
        rows,
        sheet_name,
        table_name,
        column_is_numeric,
    )
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

    #[test]
    fn decimal_text_in_a_numeric_column_becomes_a_number_cell() {
        let c = vec![col("total", "money"), col("qty", "decimal(38,10)")];
        let r = vec![
            CellValue::String("1234.56".into()),
            CellValue::String("-0.0000000001".into()),
        ];
        let text = String::from_utf8_lossy(&excel_xlsx(&c, &[r], "Results", "Results")).to_string();
        assert!(text.contains("<v>1234.56</v>"), "{text}");
        assert!(text.contains("<v>-0.0000000001</v>"), "{text}");
    }

    /// The app's classifier decides: a Redis `string` column stays text, a
    /// ClickHouse `Nullable(Decimal(10, 2))` is a number.
    #[test]
    fn the_app_classifier_picks_number_cells() {
        let c = vec![
            col("result", "string"),
            col("amount", "Nullable(Decimal(10, 2))"),
        ];
        let r = vec![
            CellValue::String("00014".into()),
            CellValue::String("7.50".into()),
        ];
        let text = String::from_utf8_lossy(&excel_xlsx(&c, &[r], "Results", "Results")).to_string();
        assert!(
            text.contains("<t xml:space=\"preserve\">00014</t>"),
            "{text}"
        );
        assert!(text.contains("<v>7.50</v>"), "{text}");
    }
}
