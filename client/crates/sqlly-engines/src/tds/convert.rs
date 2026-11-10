//! Mapping tiberius wire values to SQLLY's engine-neutral shapes: column types
//! to canonical SQL type names, and `ColumnData` cells to typed
//! [`CellValue`]s.

use sqlly_core::CellValue;
use tiberius::{ColumnData, FromSql};

/// Renders a decimal/numeric value (a signed unscaled mantissa plus a scale)
/// to a correct base-10 string.
///
/// tiberius' own `Display`/`Debug` for `Numeric` is wrong for negative values:
/// it formats `int_part()` and `dec_part()` separately with `"{}.{:0pad$}"`,
/// and because both halves are negative for a negative value it emits the sign
/// on BOTH parts — e.g. `-3790.62` prints as `-3790.-620000000`. We rebuild the
/// string from the full signed mantissa (`value`) and `scale` so the sign
/// appears exactly once and every fractional digit is preserved.
fn numeric_to_string(value: i128, scale: u8) -> String {
    let scale = scale as usize;
    if scale == 0 {
        return value.to_string();
    }
    let negative = value < 0;
    // `unsigned_abs` avoids overflow on i128::MIN that plain `abs` would hit.
    let digits = value.unsigned_abs().to_string();
    let (int_part, frac_part) = if digits.len() > scale {
        let split = digits.len() - scale;
        (digits[..split].to_string(), digits[split..].to_string())
    } else {
        // Value is purely fractional (|value| < 1): pad leading zeros so the
        // fractional part always has exactly `scale` digits.
        ("0".to_string(), format!("{digits:0>scale$}"))
    };
    format!(
        "{}{}.{}",
        if negative { "-" } else { "" },
        int_part,
        frac_part
    )
}

/// Maps a tiberius [`tiberius::ColumnType`] to a canonical SQL type name (e.g.
/// `bigint`, `decimal`, `datetime2`, `uniqueidentifier`).
///
/// Previously the column's `data_type` was filled with `format!("{:?}",
/// column.column_type())`, which leaks tiberius' internal enum spellings —
/// `Int8`, `Int2`, `Decimaln`, `Floatn`, `Bitn`, `Datetimen`, … — into the
/// result metadata. Downstream consumers (the macOS result grid's
/// type-aware formatting, the export/where-clause builders) key off SQL type
/// names, so those internal names silently disabled integer/decimal/date
/// formatting. Returning canonical names restores type-aware rendering.
pub(crate) fn column_type_to_sql_name(column_type: tiberius::ColumnType) -> &'static str {
    use tiberius::ColumnType;
    match column_type {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Datetime | ColumnType::Datetime4 | ColumnType::Datetimen => "datetime",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => "varbinary",
        ColumnType::BigVarChar | ColumnType::BigChar | ColumnType::Text => "varchar",
        ColumnType::NVarchar | ColumnType::NChar | ColumnType::NText => "nvarchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// Converts a tiberius `ColumnData` cell into a typed [`CellValue`].
///
/// Earlier revisions stringified every cell with the `Debug` formatter, which
/// surfaced internal representations like `I32(Some(1))` in the result grid.
/// This maps each variant to its semantic value: integers/floats/bools/strings
/// become their native `CellValue`, temporal and decimal types are rendered to
/// canonical string form via tiberius' `FromSql` conversions (e.g. a
/// `datetime2` becomes `2024-01-15 10:30:00`), GUIDs/XML become their string
/// form, and SQL `NULL` becomes [`CellValue::Null`].
pub(crate) fn column_data_to_cell(cell: ColumnData<'static>) -> CellValue {
    match cell {
        ColumnData::U8(v) => v
            .map(|n| CellValue::I64(n as i64))
            .unwrap_or(CellValue::Null),
        ColumnData::I16(v) => v
            .map(|n| CellValue::I64(n as i64))
            .unwrap_or(CellValue::Null),
        ColumnData::I32(v) => v
            .map(|n| CellValue::I64(n as i64))
            .unwrap_or(CellValue::Null),
        ColumnData::I64(v) => v.map(CellValue::I64).unwrap_or(CellValue::Null),
        ColumnData::F32(v) => v
            .map(|n| CellValue::F64(n as f64))
            .unwrap_or(CellValue::Null),
        ColumnData::F64(v) => v.map(CellValue::F64).unwrap_or(CellValue::Null),
        ColumnData::Bit(v) => v.map(CellValue::Bool).unwrap_or(CellValue::Null),
        ColumnData::String(ref v) => v
            .as_ref()
            .map(|s| CellValue::String(s.to_string()))
            .unwrap_or(CellValue::Null),
        ColumnData::Guid(v) => v
            .map(|g| CellValue::String(g.to_string()))
            .unwrap_or(CellValue::Null),
        ColumnData::Binary(ref v) => v
            .as_ref()
            .map(|b| CellValue::Bytes(b.to_vec()))
            .unwrap_or(CellValue::Null),
        ColumnData::Numeric(v) => v
            .map(|n| CellValue::String(numeric_to_string(n.value(), n.scale())))
            .unwrap_or(CellValue::Null),
        ColumnData::Xml(ref v) => v
            .as_ref()
            .map(|x| CellValue::String(x.to_string()))
            .unwrap_or(CellValue::Null),
        ColumnData::DateTime(_)
        | ColumnData::SmallDateTime(_)
        | ColumnData::DateTime2(_)
        | ColumnData::Date(_)
        | ColumnData::Time(_)
        | ColumnData::DateTimeOffset(_) => temporal_to_cell(cell),
    }
}

/// Renders a tiberius temporal `ColumnData` to a canonical string `CellValue`
/// using the crate's `FromSql` conversions, falling back to `NULL` when the
/// value is absent and to a last-resort `Debug` string only if no conversion
/// applies (which should not happen for the temporal variants routed here).
fn temporal_to_cell(cell: ColumnData<'static>) -> CellValue {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    match &cell {
        ColumnData::Date(_) => match NaiveDate::from_sql(&cell) {
            Ok(Some(d)) => CellValue::String(d.to_string()),
            Ok(None) => CellValue::Null,
            Err(_) => CellValue::Null,
        },
        ColumnData::Time(_) => match NaiveTime::from_sql(&cell) {
            Ok(Some(t)) => CellValue::String(t.to_string()),
            Ok(None) => CellValue::Null,
            Err(_) => CellValue::Null,
        },
        ColumnData::DateTimeOffset(_) => match DateTime::<FixedOffset>::from_sql(&cell) {
            Ok(Some(dt)) => CellValue::String(dt.to_rfc3339()),
            Ok(None) => CellValue::Null,
            Err(_) => CellValue::Null,
        },
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            match NaiveDateTime::from_sql(&cell) {
                Ok(Some(dt)) => CellValue::String(dt.to_string()),
                Ok(None) => CellValue::Null,
                // Some servers surface UTC datetimes; try a DateTime<Utc> fallback.
                Err(_) => match DateTime::<Utc>::from_sql(&cell) {
                    Ok(Some(dt)) => CellValue::String(dt.to_rfc3339()),
                    _ => CellValue::Null,
                },
            }
        }
        _ => CellValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_data_maps_to_typed_cell_values() {
        // Integers of every width collapse to I64.
        assert!(matches!(
            column_data_to_cell(ColumnData::I32(Some(1))),
            CellValue::I64(1)
        ));
        assert!(matches!(
            column_data_to_cell(ColumnData::U8(Some(255))),
            CellValue::I64(255)
        ));
        assert!(matches!(
            column_data_to_cell(ColumnData::I16(Some(-7))),
            CellValue::I64(-7)
        ));
        assert!(matches!(
            column_data_to_cell(ColumnData::I64(Some(9))),
            CellValue::I64(9)
        ));
        // Floats.
        assert!(matches!(
            column_data_to_cell(ColumnData::F64(Some(3.5))),
            CellValue::F64(v) if (v - 3.5).abs() < f64::EPSILON
        ));
        // Bit -> Bool.
        assert!(matches!(
            column_data_to_cell(ColumnData::Bit(Some(true))),
            CellValue::Bool(true)
        ));
        // String.
        match column_data_to_cell(ColumnData::String(Some("hi".into()))) {
            CellValue::String(s) => assert_eq!(s, "hi"),
            other => panic!("expected String, got {other:?}"),
        }
        // NULL of any type -> Null (no more `I32(None)` debug strings).
        assert!(matches!(
            column_data_to_cell(ColumnData::I32(None)),
            CellValue::Null
        ));
        assert!(matches!(
            column_data_to_cell(ColumnData::String(None)),
            CellValue::Null
        ));
    }

    #[test]
    fn column_data_does_not_emit_debug_wrapper_strings() {
        // Regression: a value of 1 must never render as "I32(Some(1))".
        match column_data_to_cell(ColumnData::I32(Some(1))) {
            CellValue::String(s) => panic!("integer leaked as string: {s}"),
            CellValue::I64(1) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn numeric_to_string_renders_negative_fraction_with_single_sign() {
        // Regression for tiberius' Numeric Display, which signs BOTH the
        // integer and fractional parts: -3790.62 -> "-3790.-620000000".
        // scale 9, value = -3790.620000000 unscaled.
        assert_eq!(numeric_to_string(-3_790_620_000_000, 9), "-3790.620000000");
        assert_eq!(
            numeric_to_string(-47_333_000_000_000, 9),
            "-47333.000000000"
        );
        // Positive values are unaffected.
        assert_eq!(numeric_to_string(3_790_620_000_000, 9), "3790.620000000");
    }

    #[test]
    fn numeric_to_string_handles_pure_fraction_and_zero_scale() {
        // |value| < 1 pads leading zeros to fill the scale.
        assert_eq!(numeric_to_string(620_000_000, 9), "0.620000000");
        assert_eq!(numeric_to_string(-620_000_000, 9), "-0.620000000");
        assert_eq!(numeric_to_string(6_200_000, 9), "0.006200000");
        // scale 0 is a plain integer, no decimal point.
        assert_eq!(numeric_to_string(42, 0), "42");
        assert_eq!(numeric_to_string(-42, 0), "-42");
    }

    #[test]
    fn column_type_maps_to_canonical_sql_names() {
        use tiberius::ColumnType;
        // The exact internal names that leaked into the grid before the fix.
        assert_eq!(column_type_to_sql_name(ColumnType::Decimaln), "decimal");
        assert_eq!(column_type_to_sql_name(ColumnType::Numericn), "numeric");
        assert_eq!(column_type_to_sql_name(ColumnType::Int8), "bigint");
        assert_eq!(column_type_to_sql_name(ColumnType::Int2), "smallint");
        assert_eq!(column_type_to_sql_name(ColumnType::Int4), "int");
        assert_eq!(column_type_to_sql_name(ColumnType::Int1), "tinyint");
        assert_eq!(column_type_to_sql_name(ColumnType::Bit), "bit");
        assert_eq!(column_type_to_sql_name(ColumnType::Float8), "float");
        assert_eq!(column_type_to_sql_name(ColumnType::Float4), "real");
        assert_eq!(
            column_type_to_sql_name(ColumnType::Guid),
            "uniqueidentifier"
        );
        assert_eq!(column_type_to_sql_name(ColumnType::Datetime2), "datetime2");
        assert_eq!(column_type_to_sql_name(ColumnType::Daten), "date");
        // Temporal types must surface canonical SQL names so the macOS grid's
        // type-aware formatting (and the selection-stats date Min/Max) treat
        // them as dates rather than text.
        assert_eq!(column_type_to_sql_name(ColumnType::Datetime), "datetime");
        assert_eq!(column_type_to_sql_name(ColumnType::Datetime4), "datetime");
        assert_eq!(column_type_to_sql_name(ColumnType::Datetimen), "datetime");
        assert_eq!(
            column_type_to_sql_name(ColumnType::DatetimeOffsetn),
            "datetimeoffset"
        );
        assert_eq!(column_type_to_sql_name(ColumnType::Timen), "time");
        assert_eq!(column_type_to_sql_name(ColumnType::NVarchar), "nvarchar");
    }
}
