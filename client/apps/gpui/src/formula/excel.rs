//! A formula as Excel itself writes it: `!Revenue - !Cost[1]` on the sheet
//! row 7 becomes `=C7-D6`, so an exported computed column recalculates in
//! Excel exactly like it does in the grid.

use super::parser::{Expr, binary_power};
use super::{CellRef, RowRef};

/// The sheet the formula is written into.
pub struct ExcelRefContext<'a> {
    /// The column letters (`C`) of a result column, by name.
    pub column_letter: &'a dyn Fn(&str) -> Option<String>,
    /// The sheet row of result row 0 (1-based; 2 under a header row).
    pub first_data_row: u32,
    /// Result rows written.
    pub row_count: u32,
    /// The 0-based result row being written.
    pub row: i64,
    /// The defined name a variable is written as.
    pub variable_name: &'a dyn Fn(&str) -> String,
}

/// Excel's last row: always empty here, so a reference outside the result
/// reads a blank cell exactly as the grid does.
const BLANK_ROW: u32 = 1_048_576;

/// The formula text (without the leading `=`); `None` when a reference
/// names a column the sheet does not have.
pub fn to_excel_formula(expr: &Expr, ctx: &ExcelRefContext) -> Option<String> {
    write(expr, ctx, 0)
}

fn cell(r: &CellRef, ctx: &ExcelRefContext) -> Option<String> {
    let letter = (ctx.column_letter)(&r.column)?;
    let row = r.resolve_row(ctx.row);
    let sheet_row = if row < 0 || row >= i64::from(ctx.row_count) {
        BLANK_ROW
    } else {
        ctx.first_data_row + row as u32
    };
    Some(match r.row {
        RowRef::Absolute(_) => format!("{letter}${sheet_row}"),
        RowRef::Relative(_) => format!("{letter}{sheet_row}"),
    })
}

fn write(expr: &Expr, ctx: &ExcelRefContext, parent_power: u8) -> Option<String> {
    Some(match expr {
        Expr::Number(n) => super::format_general(*n),
        Expr::Text(t) => format!("\"{}\"", t.replace('"', "\"\"")),
        Expr::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Expr::Error(e) => e.as_str().into(),
        Expr::Missing => String::new(),
        Expr::Ref(r) => cell(r, ctx)?,
        Expr::Range(a, b) => format!("{}:{}", cell(a, ctx)?, cell(b, ctx)?),
        Expr::Var(name) => (ctx.variable_name)(name),
        Expr::Neg(e) => format!("-{}", write(e, ctx, 6)?),
        Expr::Percent(e) => format!("{}%", write(e, ctx, 6)?),
        Expr::Binary(op, a, b) => {
            let power = binary_power(op).unwrap_or(0);
            let text = format!(
                "{}{}{}",
                write(a, ctx, power)?,
                op,
                // Left associative: an equal-power right side needs parens.
                write(b, ctx, power + 1)?
            );
            if power < parent_power {
                format!("({text})")
            } else {
                text
            }
        }
        Expr::Call(name, args) if name == "COALESCE" => coalesce(args, ctx)?,
        Expr::Call(name, args) => {
            let args: Option<Vec<String>> = args.iter().map(|a| write(a, ctx, 0)).collect();
            // Newer functions carry Excel's _xlfn. prefix in the file format.
            let stored = match name.as_str() {
                "IFS" | "SWITCH" | "TEXTJOIN" | "CONCAT" | "IFNA" | "XOR" | "DAYS"
                | "ISOWEEKNUM" | "CEILING.MATH" | "FLOOR.MATH" | "TEXTBEFORE" | "TEXTAFTER" => {
                    format!("_xlfn.{name}")
                }
                _ => name.clone(),
            };
            format!("{stored}({})", args?.join(","))
        }
    })
}

/// Excel has no COALESCE: the first argument that is neither blank nor ""
/// as nested IFs.
fn coalesce(args: &[Expr], ctx: &ExcelRefContext) -> Option<String> {
    let Some((last, rest)) = args.split_last() else {
        return Some("\"\"".into());
    };
    let mut out = write(last, ctx, 0)?;
    for arg in rest.iter().rev() {
        let a = write(arg, ctx, 0)?;
        out = format!("IF(({a})&\"\"<>\"\",{a},{out})");
    }
    Some(out)
}
