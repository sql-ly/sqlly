//! Scalar (formula) cells: one value computed from earlier outputs and
//! parameters by [`crate::analytics::scalar`]. The expression is the cell's
//! `source`; the spec lists the cells it reads (for the dependency graph)
//! and how the value is shown. Markdown shows a scalar with `{{name}}`, and
//! a SQL cell that references it gets the value as a literal.
//!
//! ```json
//! {"inputs": ["c_orders"], "format": {"style": "currency", "currency": "USD", "decimals": 2}}
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use super::spec::SpecExtra;
use crate::analytics::scalar::{Expr, ScalarError, ScalarValue, parse};
use crate::notebook::params::{BindError, ParamType, ParamValue, ParameterDef, render_literal};
use crate::notebook::references::{RefTarget, cell_references};
use crate::notebook::{CellKind, Completeness, Dialects, NotebookDoc};
use crate::report::format::{ValueFormat, format_cell, format_number};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScalarCellSpec {
    /// Cells the expression reads, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ValueFormat>,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

/// The cells `expr` reads, as ids of `doc`'s cells (parameters and unknown
/// names are left out; evaluation reports those).
pub fn input_ids(doc: &NotebookDoc, expr: &Expr) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in expr.references() {
        if name.starts_with("param.") {
            continue;
        }
        if let Some(cell) = doc.resolve(&name)
            && !out.contains(&cell.id)
        {
            out.push(cell.id.clone());
        }
    }
    out
}

/// Parse `source` and refresh `inputs` from it.
pub fn sync_inputs(
    doc: &NotebookDoc,
    spec: &mut ScalarCellSpec,
    source: &str,
) -> Result<Expr, ScalarError> {
    let expr = parse(source)?;
    spec.inputs = input_ids(doc, &expr);
    Ok(expr)
}

/// The value as display text with the cell's format. Exact numbers keep
/// their digits.
pub fn display(value: &ScalarValue, format: Option<&ValueFormat>) -> String {
    match (value, format) {
        (ScalarValue::Number(n), Some(format)) => format_number(*n, format),
        (ScalarValue::Integer(n), Some(format)) => format_cell(
            &CellValue::I64(*n),
            crate::report::format::ColumnKind::Integer,
            format,
        ),
        (ScalarValue::Decimal(d), Some(format)) => format_cell(
            &CellValue::String(d.clone()),
            crate::report::format::ColumnKind::Decimal,
            format,
        ),
        (ScalarValue::Null, _) => "NULL".to_string(),
        (other, Some(format)) => format_cell(
            &other.to_cell(),
            crate::report::format::ColumnKind::Text,
            format,
        ),
        (other, None) => other.display(),
    }
}

/// The value as a one-row output named `name`.
pub fn output_rows(name: &str, value: &ScalarValue) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let data_type = match value {
        ScalarValue::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => "bigint",
        ScalarValue::Number(_) => "double",
        ScalarValue::Integer(_) => "bigint",
        ScalarValue::Decimal(_) => "decimal",
        ScalarValue::Bool(_) => "boolean",
        ScalarValue::Date(_) => "timestamp",
        ScalarValue::Text(_) | ScalarValue::Null => "varchar",
    };
    (
        vec![ResultColumn {
            name: name.to_string(),
            data_type: data_type.into(),
            nullable: true,
        }],
        vec![vec![value.to_cell()]],
    )
}

/// A scalar cell's value read back from its one-row output (the inverse of
/// [`output_rows`]): integers and decimal text stay exact.
pub fn value_from_output(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Option<ScalarValue> {
    let cell = rows.first()?.first()?;
    let data_type = columns
        .first()
        .map(|c| c.data_type.as_str())
        .unwrap_or_default();
    Some(match cell {
        CellValue::Bytes(_) => ScalarValue::Null,
        other => ScalarValue::from_typed_cell(other, data_type),
    })
}

/// Whether a scalar computed into an output with `completeness` describes
/// the whole of its inputs.
pub fn is_partial(completeness: Completeness) -> bool {
    !completeness.supports_summaries()
}

/// The SQL cells of `doc` whose text binds one of the `partial` scalar
/// cells (by id), directly or through a SQL cell they reference that does:
/// their results were computed from partial data.
pub fn cells_binding_partial(
    doc: &NotebookDoc,
    dialects: &Dialects,
    partial: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut tainted: BTreeSet<String> = BTreeSet::new();
    if partial.is_empty() {
        return tainted;
    }
    loop {
        let before = tainted.len();
        for cell in doc.cells.iter().filter(|c| c.kind == CellKind::Sql) {
            if tainted.contains(&cell.id) {
                continue;
            }
            let binds = cell_references(doc, cell, dialects).iter().any(|r| {
                let RefTarget::Cell(token) = &r.target else {
                    return false;
                };
                doc.resolve(token).is_some_and(|target| {
                    partial.contains(&target.id) || tainted.contains(&target.id)
                })
            });
            if binds {
                tainted.insert(cell.id.clone());
            }
        }
        if tainted.len() == before {
            return tainted;
        }
    }
}

/// The value as a SQL literal for `db`, rendered by the parameter binder so
/// scalars and parameters quote alike. Integers and decimals keep every
/// digit.
pub fn sql_literal(db: DatabaseType, name: &str, value: &ScalarValue) -> Result<String, BindError> {
    let (ty, param) = match value {
        ScalarValue::Null => (ParamType::String, ParamValue::Null),
        ScalarValue::Integer(n) => (ParamType::Integer, ParamValue::Integer(*n)),
        ScalarValue::Decimal(d) => (ParamType::Decimal, ParamValue::Decimal(d.clone())),
        ScalarValue::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
            (ParamType::Integer, ParamValue::Integer(*n as i64))
        }
        ScalarValue::Number(n) => (
            ParamType::Decimal,
            ParamValue::Decimal(crate::report::format::float_literal(*n)),
        ),
        ScalarValue::Text(t) => (ParamType::String, ParamValue::Text(t.clone())),
        ScalarValue::Bool(b) => (ParamType::Boolean, ParamValue::Boolean(*b)),
        ScalarValue::Date(d) if crate::analytics::time::has_time_of_day(*d) => {
            (ParamType::Datetime { tz: None }, ParamValue::DateTime(*d))
        }
        ScalarValue::Date(d) => (ParamType::Date, ParamValue::Date(d.date())),
    };
    render_literal(db, &ParameterDef::new(name, ty), &param, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::OutputView;
    use crate::analytics::scalar::{MapEnv, evaluate};
    use crate::notebook::{CellKind, SeededIds};

    #[test]
    fn inputs_follow_the_expression_and_values_render() {
        let mut ids = SeededIds(3);
        let mut doc = NotebookDoc::new("t", &mut ids);
        doc.cells[0].name = Some("orders".into());
        let i = doc.add_cell(CellKind::Scalar, None, &mut ids);
        let mut spec = ScalarCellSpec::default();
        let source = "round(sum(orders.amount) / count(orders) * param.fx, 2)";
        let expr = sync_inputs(&doc, &mut spec, source).unwrap();
        assert_eq!(spec.inputs, vec![doc.cells[0].id.clone()]);
        doc.cells[i].spec = super::super::spec::write(&spec);
        assert_eq!(doc.cells[i].spec_inputs(), spec.inputs);

        let columns = vec![ResultColumn {
            name: "amount".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let rows = vec![vec![CellValue::I64(10)], vec![CellValue::I64(15)]];
        let mut env = MapEnv::default();
        env.outputs
            .insert("orders".into(), OutputView::complete(&columns, &rows));
        env.values
            .insert("param.fx".into(), ScalarValue::Number(2.0));
        let value = evaluate(&expr, &env).unwrap();
        assert_eq!(value, ScalarValue::Number(25.0));
        assert_eq!(
            display(&value, Some(&ValueFormat::currency("USD", 2))),
            "$25.00"
        );
        assert_eq!(display(&ScalarValue::Number(0.5), None), "0.5");
        let (columns, rows) = output_rows("aov", &value);
        assert_eq!(columns[0].data_type, "bigint");
        assert_eq!(rows[0][0], CellValue::I64(25));
    }

    #[test]
    fn literals_use_the_parameter_binder() {
        let text = ScalarValue::Text("O'Brien".into());
        assert_eq!(
            sql_literal(DatabaseType::SqlServer, "who", &text).unwrap(),
            "N'O''Brien'"
        );
        assert_eq!(
            sql_literal(DatabaseType::Postgres, "n", &ScalarValue::Number(-2.5)).unwrap(),
            "(-2.5)"
        );
        assert_eq!(
            sql_literal(DatabaseType::Postgres, "n", &ScalarValue::Number(7.0)).unwrap(),
            "7"
        );
        assert_eq!(
            sql_literal(DatabaseType::Postgres, "b", &ScalarValue::Bool(true)).unwrap(),
            "TRUE"
        );
        assert_eq!(
            sql_literal(DatabaseType::Postgres, "z", &ScalarValue::Null).unwrap(),
            "NULL"
        );
    }

    #[test]
    fn bigint_and_decimal_literals_keep_every_digit() {
        assert_eq!(
            sql_literal(
                DatabaseType::Postgres,
                "id",
                &ScalarValue::Integer(1234567890123456789)
            )
            .unwrap(),
            "1234567890123456789"
        );
        assert_eq!(
            sql_literal(
                DatabaseType::SqlServer,
                "amt",
                &ScalarValue::Decimal("12345678901234567.89".into())
            )
            .unwrap(),
            "12345678901234567.89"
        );
        let value = ScalarValue::Integer(1234567890123456789);
        let (columns, rows) = output_rows("id", &value);
        assert_eq!(columns[0].data_type, "bigint");
        assert_eq!(value_from_output(&columns, &rows), Some(value));
        let value = ScalarValue::Decimal("12345678901234567.89".into());
        let (columns, rows) = output_rows("amt", &value);
        assert_eq!(rows[0][0], CellValue::String("12345678901234567.89".into()));
        assert_eq!(value_from_output(&columns, &rows), Some(value.clone()));
        assert_eq!(
            display(&value, Some(&ValueFormat::currency("USD", 2))),
            "$12,345,678,901,234,567.89"
        );
    }

    #[test]
    fn sql_cells_that_bind_partial_scalars_are_found() {
        let mut ids = SeededIds(9);
        let mut doc = NotebookDoc::new("t", &mut ids);
        doc.cells[0].name = Some("orders".into());
        let total = doc.add_cell(CellKind::Scalar, None, &mut ids);
        doc.cells[total].name = Some("total".into());
        let direct = doc.add_cell(CellKind::Sql, None, &mut ids);
        doc.cells[direct].name = Some("big".into());
        doc.cells[direct].source = "SELECT * FROM t WHERE amount > {{total}}".into();
        let indirect = doc.add_cell(CellKind::Sql, None, &mut ids);
        doc.cells[indirect].source = "SELECT count(*) FROM {{big}}".into();
        let unrelated = doc.add_cell(CellKind::Sql, None, &mut ids);
        doc.cells[unrelated].source = "SELECT 1".into();
        let dialects = Dialects::single(DatabaseType::Postgres);
        let partial = BTreeSet::from([doc.cells[total].id.clone()]);
        let found = cells_binding_partial(&doc, &dialects, &partial);
        assert_eq!(
            found,
            BTreeSet::from([doc.cells[direct].id.clone(), doc.cells[indirect].id.clone()])
        );
        assert!(cells_binding_partial(&doc, &dialects, &BTreeSet::new()).is_empty());
        assert!(is_partial(Completeness::Truncated { cap: 2 }));
        assert!(is_partial(Completeness::DerivedFromPartial));
        assert!(!is_partial(Completeness::Spilled { total: 9 }));
    }
}
