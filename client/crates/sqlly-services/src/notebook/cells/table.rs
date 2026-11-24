//! Table cells: a formatted grid over another cell's output. Formats,
//! conditional colors, computed columns and the column arrangement (order,
//! hidden and frozen columns, sort, grouping, column filters) are kept as
//! `-- sqlly` directive lines, the same grammar a query tab writes, so the
//! results grid that renders a query renders the table cell too.
//!
//! ```json
//! {"input": "c_orders", "search": "germany",
//!  "directives": ["-- sqlly format: column=revenue; currency",
//!                 "-- sqlly grid: freeze=1; sort=revenue desc",
//!                 "-- sqlly conditional: column=revenue; when=< 0; text=#C62828"]}
//! ```

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::spec::SpecExtra;

/// Default table height in the notebook, in logical pixels.
pub const DEFAULT_HEIGHT: f32 = 300.0;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableCellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    /// `-- sqlly` directive lines, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<String>,
    /// Rows must contain this text in some column (case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

impl TableCellSpec {
    /// The directives as one text block, one per line.
    pub fn directive_text(&self) -> String {
        self.directives.join("\n")
    }

    /// Keep the `--` comment lines of `text` as the directives; anything
    /// else (SQL, blank lines) is dropped, since nothing here runs.
    pub fn set_directive_text(&mut self, text: &str) {
        self.directives = text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("--"))
            .map(str::to_string)
            .collect();
    }

    pub fn height(&self) -> f32 {
        self.height.unwrap_or(DEFAULT_HEIGHT).clamp(140.0, 900.0)
    }
}

fn cell_text(cell: &CellValue) -> Option<String> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::String(s) => Some(s.clone()),
        CellValue::Bytes(_) => None,
    }
}

/// The rows where some cell contains `query` (case-insensitive). An empty
/// query keeps every row.
pub fn search_rows(rows: &[Vec<CellValue>], query: &str) -> Vec<Vec<CellValue>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|row| {
            row.iter()
                .filter_map(cell_text)
                .any(|text| text.to_lowercase().contains(&needle))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_keep_only_comment_lines_and_round_trip() {
        let mut spec = TableCellSpec {
            input: Some("c_1".into()),
            ..TableCellSpec::default()
        };
        spec.set_directive_text(
            "-- sqlly format: column=revenue; currency\r\nSELECT 1\n\n  -- sqlly grid: freeze=1\n",
        );
        assert_eq!(
            spec.directives,
            [
                "-- sqlly format: column=revenue; currency",
                "-- sqlly grid: freeze=1"
            ]
        );
        assert_eq!(
            spec.directive_text(),
            "-- sqlly format: column=revenue; currency\n-- sqlly grid: freeze=1"
        );
        let json = super::super::spec::write(&spec);
        assert_eq!(super::super::spec::read::<TableCellSpec>(&json), spec);
        assert_eq!(spec.height(), DEFAULT_HEIGHT);
    }

    #[test]
    fn search_matches_any_cell_case_insensitively() {
        let rows = vec![
            vec![CellValue::String("Germany".into()), CellValue::I64(4)],
            vec![CellValue::String("France".into()), CellValue::I64(40)],
            vec![CellValue::Null, CellValue::F64(1.5)],
        ];
        assert_eq!(search_rows(&rows, "germ").len(), 1);
        assert_eq!(search_rows(&rows, "4").len(), 2);
        assert_eq!(search_rows(&rows, "  ").len(), 3);
        assert!(search_rows(&rows, "spain").is_empty());
    }
}
