//! Transpose a resident result set: source columns become rows and source
//! rows become columns (`Column`, `Row 1`, `Row 2`, …).
//!
//! Pure and GPUI-free. The host builds a second `SqllyDataTable` from the
//! returned columns/rows via `grid_bridge::build_grid_data`.

use sqlly_core::{CellValue, ResultColumn};

/// Maximum source rows turned into columns. A transposed grid past this is
/// unusable (each source row is a column).
pub const TRANSPOSE_MAX_ROWS: usize = 1_000;

/// Transpose `rows` (capped at `cap`) so each source column becomes one
/// output row. Returns `(columns, rows, truncated)`.
pub fn transpose(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    cap: usize,
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>, bool) {
    let truncated = rows.len() > cap;
    let used = rows.len().min(cap);
    let mut out_columns = Vec::with_capacity(used + 1);
    out_columns.push(ResultColumn {
        name: crate::localization::text("results.transpose.source_column_header"),
        data_type: "text".into(),
        nullable: false,
    });
    for i in 0..used {
        out_columns.push(ResultColumn {
            name: crate::localization::format(
                "results.transpose.source_row_header",
                &[("row_number", &(i + 1).to_string())],
            ),
            data_type: "text".into(),
            nullable: true,
        });
    }
    let mut out_rows = Vec::with_capacity(columns.len());
    for (ci, column) in columns.iter().enumerate() {
        let mut row = Vec::with_capacity(used + 1);
        row.push(CellValue::String(column.name.clone()));
        for source in rows.iter().take(used) {
            row.push(source.get(ci).cloned().unwrap_or(CellValue::Null));
        }
        out_rows.push(row);
    }
    (out_columns, out_rows, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: true,
        }
    }

    #[test]
    fn three_by_two_becomes_three_rows_with_column_header() {
        let columns = vec![col("id"), col("name"), col("qty")];
        let rows = vec![
            vec![
                CellValue::I64(1),
                CellValue::String("a".into()),
                CellValue::I64(10),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("b".into()),
                CellValue::I64(20),
            ],
        ];
        let (t_cols, t_rows, truncated) = transpose(&columns, &rows, TRANSPOSE_MAX_ROWS);
        assert!(!truncated);
        assert_eq!(
            t_cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Column", "Row 1", "Row 2"]
        );
        assert_eq!(t_rows.len(), 3);
        assert_eq!(t_rows[0][0], CellValue::String("id".into()));
        assert_eq!(t_rows[0][1], CellValue::I64(1));
        assert_eq!(t_rows[0][2], CellValue::I64(2));
        assert_eq!(t_rows[1][0], CellValue::String("name".into()));
    }

    #[test]
    fn zero_rows_keeps_only_the_column_header() {
        let columns = vec![col("id"), col("name"), col("qty")];
        let (t_cols, t_rows, truncated) = transpose(&columns, &[], TRANSPOSE_MAX_ROWS);
        assert!(!truncated);
        assert_eq!(t_cols.len(), 1);
        assert_eq!(t_cols[0].name, "Column");
        assert_eq!(t_rows.len(), 3);
        assert_eq!(t_rows[0], vec![CellValue::String("id".into())]);
        assert_eq!(t_rows[1], vec![CellValue::String("name".into())]);
        assert_eq!(t_rows[2], vec![CellValue::String("qty".into())]);
    }

    #[test]
    fn cap_truncates_and_sets_flag() {
        let columns = vec![col("id")];
        let rows: Vec<Vec<CellValue>> = (0..5).map(|i| vec![CellValue::I64(i)]).collect();
        let (t_cols, t_rows, truncated) = transpose(&columns, &rows, 3);
        assert!(truncated);
        assert_eq!(t_cols.len(), 4); // Column + 3 rows
        assert_eq!(t_rows.len(), 1);
        assert_eq!(t_rows[0].len(), 4);
    }
}
