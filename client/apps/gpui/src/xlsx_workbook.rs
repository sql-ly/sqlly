//! A styled, multi-sheet `.xlsx` writer — the export that keeps what the
//! grid shows: column order, widths, hidden and frozen columns, number and
//! date formats, conditional formatting, and computed columns as live Excel
//! formulas (with their computed values cached, so the file reads correctly
//! even before Excel recalculates).
//!
//! The writer lives in `sqlly_services::report::xlsx`, shared with notebook
//! reports and the CLI; this module re-exports it and maps the grid's
//! conditional-formatting rules onto the writer's own.

use sqlly_datatable::{ConditionalCondition, ConditionalKind, RgbaColor};
use sqlly_services::report::xlsx as core;

pub use core::{
    Cell, ConditionalBlock, Sheet, SheetColumn, Workbook, defined_name, excel_width,
    strftime_to_excel, write,
};

fn color(c: RgbaColor) -> core::RgbaColor {
    core::RgbaColor::new(c.r, c.g, c.b, c.a)
}

/// The grid's conditional-format rule as the writer's.
pub fn conditional(kind: &ConditionalKind) -> core::ConditionalRule {
    match kind {
        ConditionalKind::Rule { condition, style } => core::ConditionalRule::Rule {
            condition: match condition {
                ConditionalCondition::IsNull => core::ConditionalCondition::IsNull,
                ConditionalCondition::IsNotNull => core::ConditionalCondition::IsNotNull,
                ConditionalCondition::GreaterThan(n) => core::ConditionalCondition::GreaterThan(*n),
                ConditionalCondition::LessThan(n) => core::ConditionalCondition::LessThan(*n),
                ConditionalCondition::Between(a, b) => core::ConditionalCondition::Between(*a, *b),
                ConditionalCondition::EqualsText(t) => {
                    core::ConditionalCondition::EqualsText(t.clone())
                }
                ConditionalCondition::ContainsText(t) => {
                    core::ConditionalCondition::ContainsText(t.clone())
                }
            },
            style: core::ConditionalStyle {
                foreground: style.foreground.map(color),
                background: style.background.map(color),
                bold: style.bold,
            },
        },
        ConditionalKind::ColorScale { min, max } => core::ConditionalRule::ColorScale {
            min: color(*min),
            max: color(*max),
        },
        ConditionalKind::DataBar { color: c } => {
            core::ConditionalRule::DataBar { color: color(*c) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_rules_map_onto_the_writer() {
        let rule = conditional(&ConditionalKind::Rule {
            condition: ConditionalCondition::Between(3.0, 1.0),
            style: sqlly_datatable::ConditionalStyle {
                foreground: Some(RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                background: None,
                bold: true,
            },
        });
        assert_eq!(
            rule,
            core::ConditionalRule::Rule {
                condition: core::ConditionalCondition::Between(3.0, 1.0),
                style: core::ConditionalStyle {
                    foreground: Some(core::RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                    background: None,
                    bold: true,
                },
            }
        );
        let bytes = write(&Workbook {
            sheets: vec![Sheet {
                name: "S".into(),
                header_rows: vec![vec!["n".into()]],
                rows: vec![vec![Cell::Number(2.0)]],
                conditional: vec![ConditionalBlock {
                    column: 0,
                    rules: vec![conditional(&ConditionalKind::DataBar {
                        color: RgbaColor::new(0.0, 0.0, 1.0, 1.0),
                    })],
                }],
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(String::from_utf8_lossy(&bytes).contains("type=\"dataBar\""));
    }
}
