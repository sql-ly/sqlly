//! Pivot cells: a pivot layout over another cell's output. The layout
//! mirrors the grid's pivot configuration field for field, with columns
//! named rather than numbered so it survives a reordered `SELECT` list.
//!
//! ```json
//! {"input": "c_orders",
//!  "pivot": {"rows": ["country"], "columns": ["year"], "value": "revenue",
//!            "aggregation": "sum", "show_row_grand_total": true}}
//! ```

use serde::{Deserialize, Serialize};

use super::spec::SpecExtra;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PivotCellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    #[serde(default)]
    pub pivot: PivotLayout,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PivotAggregation {
    Count,
    #[default]
    Sum,
    Avg,
    Min,
    Max,
    First,
}

fn yes() -> bool {
    true
}

fn blank() -> String {
    "(blank)".into()
}

/// The grid pivot's configuration by column name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PivotLayout {
    #[serde(default)]
    pub rows: Vec<String>,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub aggregation: PivotAggregation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
    #[serde(default)]
    pub flat_rows: bool,
    #[serde(default = "yes")]
    pub show_row_subtotals: bool,
    #[serde(default)]
    pub show_column_subtotals: bool,
    #[serde(default = "yes")]
    pub show_row_grand_total: bool,
    #[serde(default = "yes")]
    pub show_column_grand_total: bool,
    #[serde(default = "blank")]
    pub blank_label: String,
}

impl Default for PivotLayout {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            columns: Vec::new(),
            value: None,
            aggregation: PivotAggregation::Sum,
            filters: Vec::new(),
            flat_rows: false,
            show_row_subtotals: true,
            show_column_subtotals: false,
            show_row_grand_total: true,
            show_column_grand_total: true,
            blank_label: blank(),
        }
    }
}

impl PivotLayout {
    /// Whether the layout names a value and at least one axis.
    pub fn is_ready(&self) -> bool {
        self.value.is_some() && (!self.rows.is_empty() || !self.columns.is_empty())
    }

    /// Every column the layout names.
    pub fn columns_used(&self) -> impl Iterator<Item = &str> {
        self.rows
            .iter()
            .chain(&self.columns)
            .chain(self.value.iter())
            .chain(&self.filters)
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_round_trip_with_grid_defaults() {
        let spec: PivotCellSpec = super::super::spec::read(&serde_json::json!({
            "input": "c_orders",
            "pivot": {"rows": ["country"], "columns": ["year"], "value": "revenue"}
        }));
        assert!(spec.pivot.is_ready());
        assert_eq!(spec.pivot.aggregation, PivotAggregation::Sum);
        assert!(spec.pivot.show_row_subtotals && spec.pivot.show_column_grand_total);
        assert_eq!(spec.pivot.blank_label, "(blank)");
        let written = super::super::spec::write(&spec);
        assert_eq!(super::super::spec::read::<PivotCellSpec>(&written), spec);
        assert_eq!(
            spec.pivot.columns_used().collect::<Vec<_>>(),
            ["country", "year", "revenue"]
        );
        assert!(!PivotLayout::default().is_ready());
    }
}
