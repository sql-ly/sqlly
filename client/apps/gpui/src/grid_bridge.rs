//! Bridge between sqlly-core result types and sqlly-datatable grid types.
//!
//! Maps `CellValue`/`ResultColumn`/`ResultFormattingPreferences`/`Theme` into
//! the types the `SqllyDataTable` widget expects. Pure, no I/O — fully
//! unit-testable per AGENTS.md.

use sqlly_core::{CellValue as CoreCell, DatabaseType, ResultColumn};
use sqlly_datatable::{
    BooleanFormat as GridBoolFmt, CellValue as GridCell, Column, ColumnKind, ColumnOverride,
    ConditionalCondition, ConditionalKind, ConditionalRule, ConditionalStyle,
    DateFormat as GridDateFmt, GridConfig, GridData, GridDataError, GridTheme, LabelMode,
    MetricLabel, MetricReduction, NullFormat, NumberFormat, ReplacementRule, ReplacementTiming,
    RgbaColor, StringFormat as GridStringFmt, TextAlignment,
};

use crate::preferences::{
    BoolFormatStyle, ColumnFormat, DateFormat, DecimalFormat, IntegerFormat, NegativeFormatStyle,
    ResultFormattingPreferences, ResultKind, ResultTextAlignment, TypeColorKind, TypeColorStyle,
    UuidFormat,
};
use crate::query_format::{
    BinaryFormat, QueryFormatHint, binary_format_for_column, date_pattern_for_column,
    display_format_for_column,
};
use crate::result_cell_format::{
    TypeFamily, display_transform, infer_result_kind_for, type_family,
};
use crate::theme::Theme;

/// Hand every grid's right-click menu to the app's tooltip policy, so the
/// menu suppresses tooltips for as long as it is up — the same rule the
/// explorer, the sidebar and the editor follow. Pass by name to
/// `SqllyDataTableBuilder::context_menu_visibility`.
///
/// The grid reports edges (open, then close, however the menu was dismissed),
/// which is what keeps the app's open-menu count balanced.
pub(crate) fn grid_menu_tooltip_policy(open: bool, cx: &mut gpui::App) {
    if open {
        crate::tooltip::menu_opened(cx);
    } else {
        crate::tooltip::menu_closed(cx);
    }
}

/// Floor for a column's starting width, in logical pixels. Short names still
/// get a column wide enough to hold a value.
const MIN_COLUMN_WIDTH: f32 = 150.0;

/// Ceiling for a name-derived starting width. A 200-character alias is a column
/// the user resizes, not one that pushes every other column off the viewport.
const MAX_COLUMN_WIDTH: f32 = 420.0;

/// Fixed pixels the grid's header spends on things that are not the label: an
/// 8px left pad, and ~28px on the right reserved for the sort/filter
/// affordances that the header truncates the label to clear.
const HEADER_CHROME_WIDTH: f32 = 40.0;

/// Advance width of one character as a fraction of the font size, for the
/// monospace face the grid paints with (Menlo / Consolas / DejaVu Sans Mono).
/// Mirrors `sqlly_datatable`'s own `default_char_width`, which is crate-private.
const CHAR_ADVANCE_RATIO: f32 = 0.6022;

/// A column's starting width: wide enough for its whole name at the current
/// interface size, and never narrower than [`MIN_COLUMN_WIDTH`].
///
/// The old flat 150px truncated any header longer than about seventeen
/// characters — `TransactionSettlementDate` arrived as `TransactionSettl…` and
/// stayed that way until every column was dragged by hand. Only the *starting*
/// width: a column the user has resized keeps whatever width they gave it,
/// because the resize writes to the same field and nothing recomputes this.
#[must_use]
pub fn column_width_for_name(name: &str) -> f32 {
    let advance = Theme::font_size() * CHAR_ADVANCE_RATIO;
    let label = text_cells(name, DEFAULT_TAB_CELLS) as f32 * advance;
    (label + HEADER_CHROME_WIDTH).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

/// Narrowest a column auto-sizes to, in characters of content.
const AUTO_SIZE_MIN_CHARS: usize = 3;

/// Widest any column auto-sizes to when the character cap is off (0).
const AUTO_SIZE_ABSOLUTE_MAX: f32 = 4000.0;

/// Cells a tab occupies when no editor tab width is known.
pub const DEFAULT_TAB_CELLS: usize = 4;

/// A column's width after a run, sized up or down to fit its name and its
/// widest value (in display cells, `value_cells`), but no wider than
/// `max_chars` cells of content — 0 means no cap. Whatever is longer, value
/// or name, is drawn cut off with an ellipsis.
#[must_use]
pub fn auto_column_width(name: &str, value_cells: usize, max_chars: u32) -> f32 {
    let mut chars = text_cells(name, DEFAULT_TAB_CELLS)
        .max(value_cells)
        .max(AUTO_SIZE_MIN_CHARS);
    if max_chars > 0 {
        chars = chars.min(max_chars as usize).max(AUTO_SIZE_MIN_CHARS);
    }
    let advance = Theme::font_size() * CHAR_ADVANCE_RATIO;
    (chars as f32 * advance + HEADER_CHROME_WIDTH).min(AUTO_SIZE_ABSOLUTE_MAX)
}

/// Display cells of the widest line among `values`, as the grid shows them
/// (a multi-line value is drawn one line at a time). East Asian wide and
/// full-width characters and most emoji take two cells, combining marks
/// none, and a tab `tab_cells`; a plain `chars().count()` sized a CJK
/// column about half as wide as its text and ellipsized every value.
#[must_use]
pub fn widest_value_cells<'a>(
    values: impl IntoIterator<Item = &'a str>,
    tab_cells: usize,
) -> usize {
    values
        .into_iter()
        .map(|v| {
            v.lines()
                .map(|l| text_cells(l, tab_cells))
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

/// Display cells `text` takes on one grid line, tabs counted as `tab_cells`.
#[must_use]
pub fn text_cells(text: &str, tab_cells: usize) -> usize {
    use unicode_width::UnicodeWidthStr as _;
    text.split('\t').map(|part| part.width()).sum::<usize>()
        + text.matches('\t').count() * tab_cells
}

// ---------------------------------------------------------------------------
// Cell value conversion
// ---------------------------------------------------------------------------

/// Convert a sqlly-core cell value to a sqlly-datatable cell value.
#[must_use]
pub fn convert_cell(value: &CoreCell) -> GridCell {
    match value {
        CoreCell::Null => GridCell::None,
        CoreCell::Bool(b) => GridCell::Boolean(*b),
        CoreCell::I64(v) => GridCell::Integer(*v),
        CoreCell::F64(v) => GridCell::Decimal(*v),
        CoreCell::String(s) => GridCell::Text(s.clone()),
        CoreCell::Bytes(b) => GridCell::Text(crate::localization::format(
            "results.grid.binary_cell_placeholder",
            &[("byte_count", &b.len().to_string())],
        )),
    }
}

/// Per-column kind + binary format, computed once per result set so cell
/// loops do not re-derive the type name or rescan format hints per cell.
/// Per-column cell conversion plan: the grid column kind, an optional binary
/// rendering hint, an optional display-transform format (mask/epoch/…), and —
/// for datetime columns — the effective `DateFormat` whose display time zone
/// and pattern re-render the engine's text.
struct CellPlan {
    kind: ColumnKind,
    binary: Option<BinaryFormat>,
    display: Option<ColumnFormat>,
    /// Only values that look like a 2000–2100 epoch get `display` (the
    /// "By name and value range" timestamp detection); others stay as is.
    display_epoch_range_only: bool,
    date: Option<DateFormat>,
    /// A `date=` directive: its `strftime` pattern plus the column's
    /// effective `DateFormat` (for the display time zone). Wins over `date`.
    date_directive: Option<(String, DateFormat)>,
}

fn column_cell_plan(
    db: Option<DatabaseType>,
    columns: &[ResultColumn],
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Vec<CellPlan> {
    use crate::preferences::EpochAutoDetect;
    use crate::result_cell_format::{effective_override_date_format, resolve_epoch_zone};
    let global_date = formatting
        .map(|prefs| prefs.defaults.date_format.clone())
        .unwrap_or_default();
    columns
        .iter()
        .map(|column| {
            let override_fmt =
                formatting.and_then(|prefs| prefs.column_overrides.get(&column.name));
            let directive_zone = crate::query_format::directive_zone_for(&column.name, hints);
            let mut display = display_format_for_column(column, hints).or_else(|| {
                override_fmt
                    .filter(|f| {
                        matches!(
                            f.kind,
                            ResultKind::Mask
                                | ResultKind::Epoch
                                | ResultKind::JsonPath
                                | ResultKind::Template
                                | ResultKind::Number
                        )
                    })
                    .cloned()
            });
            let inferred = infer_result_kind_for(db, Some(column));
            // "Show integer timestamps as dates": only for integer columns
            // nothing explicit (override or directive) already formats.
            let auto_epoch = formatting.map_or(EpochAutoDetect::Off, |p| p.epoch_auto_detect);
            let mut display_epoch_range_only = false;
            if display.is_none()
                && auto_epoch != EpochAutoDetect::Off
                && inferred == ResultKind::Integer
                && override_fmt.is_none_or(|o| o.is_inherit())
                && !hints.iter().any(|hint| {
                    hint.column.eq_ignore_ascii_case(&column.name)
                        && hint.format != crate::query_format::QueryColumnFormat::Keep
                })
                && crate::result_cell_format::epoch_name_candidate(&column.name)
            {
                display = Some(ColumnFormat::default_for_kind(ResultKind::Epoch));
                display_epoch_range_only = auto_epoch == EpochAutoDetect::ByNameAndRange;
            }
            if let Some(fmt) = display.as_mut().filter(|f| f.kind == ResultKind::Epoch) {
                if let Some(zone) = &directive_zone {
                    fmt.epoch_format.time_zone_mode = Some(match zone.mode {
                        crate::preferences::DateTimeZoneMode::NativeAsStored => {
                            crate::preferences::DateTimeZoneMode::AlwaysUtc
                        }
                        ref mode => mode.clone(),
                    });
                    fmt.epoch_format.time_zone_identifier = zone.identifier.clone();
                }
                resolve_epoch_zone(&mut fmt.epoch_format, &global_date);
            }
            let with_directive_zone = |mut fmt: DateFormat| {
                if let Some(zone) = &directive_zone {
                    zone.apply_to_date_format(&mut fmt);
                }
                fmt
            };
            let date_directive = date_pattern_for_column(column, hints).map(|pattern| {
                let zone =
                    with_directive_zone(effective_override_date_format(override_fmt, &global_date));
                (pattern.to_string(), zone)
            });
            CellPlan {
                kind: result_kind_to_column_kind(inferred),
                binary: binary_format_for_column(column, hints),
                display,
                display_epoch_range_only,
                date: formatting
                    .and_then(|prefs| date_display_format(column, inferred, prefs))
                    .map(with_directive_zone),
                date_directive,
            }
        })
        .collect()
}

/// The effective `DateFormat` for a datetime column, or `None` when the
/// column's cells should not be re-rendered: non-date columns, and date-only
/// or time-only columns (shifting those across zones is meaningless).
fn date_display_format(
    column: &ResultColumn,
    inferred: ResultKind,
    prefs: &ResultFormattingPreferences,
) -> Option<DateFormat> {
    use crate::result_cell_format::{ResultTemporalKind, temporal_kind};
    let override_fmt = prefs.column_overrides.get(&column.name);
    let kind = override_fmt
        .filter(|o| !o.is_inherit())
        .map(|o| o.kind)
        .unwrap_or(inferred);
    if kind != ResultKind::Date {
        return None;
    }
    if matches!(
        temporal_kind(Some(column)),
        ResultTemporalKind::Date | ResultTemporalKind::Time
    ) {
        return None;
    }
    Some(crate::result_cell_format::effective_override_date_format(
        override_fmt,
        &prefs.defaults.date_format,
    ))
}

/// Apply a plan's display transform to one raw cell (`None` = show as is).
fn plan_display_transform(raw: &CoreCell, plan: &CellPlan) -> Option<CoreCell> {
    let fmt = plan.display.as_ref()?;
    if plan.display_epoch_range_only && !crate::result_cell_format::plausible_epoch_cell(raw) {
        return None;
    }
    display_transform(raw, fmt)
}

/// The TDS layer decodes `decimal`/`numeric`/`money` cells into canonical
/// strings (see `sqlly-tds::column_data_to_cell`) so the wire value survives
/// transit without float rounding. The grid needs real numeric cells though:
/// the pivot's Sum/Avg skip `Text` cells entirely and number-format
/// preferences only apply to numeric cells. Parse `Text` cells in
/// numeric-kind columns back into numbers; anything unparseable stays text.
/// Values `f64` cannot hold digit for digit (wide `decimal(38,s)`, integers
/// past `i64`) become exact decimals so the grid paints the wire digits.
fn coerce_numeric_text(cell: GridCell, kind: ColumnKind) -> GridCell {
    let GridCell::Text(s) = &cell else {
        return cell;
    };
    if !matches!(kind, ColumnKind::Decimal | ColumnKind::Integer) {
        return cell;
    }
    let trimmed = s.trim();
    if let Some(number) = GridCell::from_decimal_text(trimmed) {
        return number;
    }
    if let Ok(v) = trimmed.parse::<f64>() {
        return GridCell::Decimal(v);
    }
    cell
}

/// A 16-byte value as UUID or ULID text; any other length falls back to the
/// `binary=hex` rendering so nothing is hidden or misread as an id.
fn binary_id_text(bytes: &[u8], format: BinaryFormat) -> String {
    let id = match format {
        BinaryFormat::Uuid => crate::binary_ids::uuid_text(bytes),
        BinaryFormat::Ulid => crate::binary_ids::ulid_text(bytes),
        BinaryFormat::Hex | BinaryFormat::Base64 => None,
    };
    id.unwrap_or_else(|| crate::binary_ids::hex_text(bytes))
}

fn convert_cell_with_binary(value: &CoreCell, binary: Option<BinaryFormat>) -> GridCell {
    match (value, binary) {
        (CoreCell::Bytes(bytes), Some(BinaryFormat::Hex)) => GridCell::Text(
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(""),
        ),
        (CoreCell::Bytes(bytes), Some(BinaryFormat::Base64)) => {
            use base64::Engine as _;
            GridCell::Text(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        (CoreCell::Bytes(bytes), Some(format @ (BinaryFormat::Uuid | BinaryFormat::Ulid))) => {
            GridCell::Text(binary_id_text(bytes, format))
        }
        _ => convert_cell(value),
    }
}

/// Convert a whole row of sqlly-core cells. Rows shorter than `col_count` are
/// padded with `None` so the result is always rectangular.
#[must_use]
#[cfg(test)]
pub fn convert_row(row: &[CoreCell], col_count: usize) -> Vec<GridCell> {
    let mut cells: Vec<GridCell> = (0..col_count)
        .map(|i| row.get(i).map(convert_cell).unwrap_or(GridCell::None))
        .collect();
    // Defensive: if the row is longer than expected, truncate.
    cells.truncate(col_count);
    cells
}

/// Convert a sqlly-core cell value, consuming it: `String` payloads move
/// instead of cloning — the per-cell hot allocation on the spill paging
/// paths, whose pages are owned and dropped right after conversion.
#[must_use]
pub fn convert_cell_owned(value: CoreCell) -> GridCell {
    match value {
        CoreCell::Null => GridCell::None,
        CoreCell::Bool(b) => GridCell::Boolean(b),
        CoreCell::I64(v) => GridCell::Integer(v),
        CoreCell::F64(v) => GridCell::Decimal(v),
        CoreCell::String(s) => GridCell::Text(s),
        CoreCell::Bytes(b) => GridCell::Text(crate::localization::format(
            "results.grid.binary_cell_placeholder",
            &[("byte_count", &b.len().to_string())],
        )),
    }
}

/// Convert an owned row, consuming it. Pads/truncates to `col_count` so the
/// result is always rectangular, like [`convert_row`].
#[must_use]
#[cfg(test)]
pub fn convert_row_owned(row: Vec<CoreCell>, col_count: usize) -> Vec<GridCell> {
    let mut cells: Vec<GridCell> = row
        .into_iter()
        .take(col_count)
        .map(convert_cell_owned)
        .collect();
    while cells.len() < col_count {
        cells.push(GridCell::None);
    }
    cells
}

// ---------------------------------------------------------------------------
// Column conversion
// ---------------------------------------------------------------------------

/// Convert a sqlly-core `ResultColumn` to a sqlly-datatable `Column`,
/// inferring the kind from the declared data type as read on `db`, the
/// engine the result came from (`None` when it is not known, as for a
/// grid the app built itself).
#[must_use]
pub fn convert_column(db: Option<DatabaseType>, col: &ResultColumn) -> Column {
    let kind = result_kind_to_column_kind(infer_result_kind_for(db, Some(col)));
    Column {
        width: column_width_for_name(&col.name),
        name: col.name.clone(),
        kind,
    }
}

/// Map the app's `ResultKind` to the grid's `ColumnKind`.
#[must_use]
pub fn result_kind_to_column_kind(kind: ResultKind) -> ColumnKind {
    match kind {
        ResultKind::Date => ColumnKind::Date,
        ResultKind::Bool => ColumnKind::Boolean,
        ResultKind::Integer => ColumnKind::Integer,
        ResultKind::Decimal => ColumnKind::Decimal,
        ResultKind::Uuid
        | ResultKind::Text
        | ResultKind::Mask
        | ResultKind::Epoch
        | ResultKind::JsonPath
        | ResultKind::Template
        | ResultKind::Number => ColumnKind::Text,
    }
}

// ---------------------------------------------------------------------------
// GridData construction
// ---------------------------------------------------------------------------

/// Build a `GridData` snapshot from a result set's columns and rows. Pads
/// ragged rows to be rectangular. The caller provides the column count and
/// the raw core cells.
///
/// # Errors
/// Returns `GridDataError` only if `columns` is non-empty and a row is
/// somehow still ragged after padding (should not happen).
fn convert_row_with_plan(row: &[CoreCell], plan: &[CellPlan]) -> Vec<GridCell> {
    plan.iter()
        .enumerate()
        .map(|(index, entry)| {
            let Some(raw) = row.get(index) else {
                return GridCell::None;
            };
            let transformed = plan_display_transform(raw, entry);
            let value = transformed.as_ref().unwrap_or(raw);
            apply_date_display(
                coerce_numeric_text(convert_cell_with_binary(value, entry.binary), entry.kind),
                entry,
            )
        })
        .collect()
}

/// Re-render a datetime cell's text in the column's display time zone and
/// format. A `date=` directive's pattern wins and also formats date-only
/// values; otherwise only Date-plan columns carry a `DateFormat`. Text the
/// formatter cannot improve on (unparseable, "as stored", a date-only value
/// without a directive) passes through.
fn apply_date_display(cell: GridCell, plan: &CellPlan) -> GridCell {
    let GridCell::Text(raw) = &cell else {
        return cell;
    };
    let rendered = match (&plan.date_directive, &plan.date) {
        (Some((pattern, zone)), _) => {
            crate::result_cell_format::format_date_text_with_pattern(raw, pattern, zone)
        }
        (None, Some(fmt)) => crate::result_cell_format::format_datetime_text(raw, fmt),
        (None, None) => None,
    };
    match rendered {
        Some(rendered) => GridCell::Text(rendered),
        None => cell,
    }
}

fn convert_cell_owned_with_binary(value: CoreCell, binary: Option<BinaryFormat>) -> GridCell {
    match (value, binary) {
        (CoreCell::Bytes(bytes), Some(BinaryFormat::Hex)) => GridCell::Text(
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(""),
        ),
        (CoreCell::Bytes(bytes), Some(BinaryFormat::Base64)) => {
            use base64::Engine as _;
            GridCell::Text(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        (CoreCell::Bytes(bytes), Some(format @ (BinaryFormat::Uuid | BinaryFormat::Ulid))) => {
            GridCell::Text(binary_id_text(&bytes, format))
        }
        (value, _) => convert_cell_owned(value),
    }
}

fn convert_row_owned_with_plan(row: Vec<CoreCell>, plan: &[CellPlan]) -> Vec<GridCell> {
    let mut iter = row.into_iter();
    plan.iter()
        .map(|entry| {
            let cell = iter.next().unwrap_or(CoreCell::Null);
            let transformed = plan_display_transform(&cell, entry);
            let value = transformed.unwrap_or(cell);
            apply_date_display(
                coerce_numeric_text(
                    convert_cell_owned_with_binary(value, entry.binary),
                    entry.kind,
                ),
                entry,
            )
        })
        .collect()
}

pub fn build_grid_data(
    db: Option<DatabaseType>,
    columns: &[ResultColumn],
    rows: &[Vec<CoreCell>],
) -> Result<GridData, GridDataError> {
    let grid_columns: Vec<Column> = columns.iter().map(|c| convert_column(db, c)).collect();
    let plan = column_cell_plan(db, columns, &[], None);
    let grid_rows: Vec<Vec<GridCell>> = rows
        .iter()
        .map(|row| convert_row_with_plan(row, &plan))
        .collect();
    GridData::new(grid_columns, grid_rows)
}

#[must_use]
#[cfg(test)]
pub fn convert_row_with_formats(
    db: Option<DatabaseType>,
    row: &[CoreCell],
    columns: &[ResultColumn],
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Vec<GridCell> {
    convert_row_with_plan(row, &column_cell_plan(db, columns, hints, formatting))
}

#[must_use]
#[allow(dead_code)] // borrowed variant retained for tests; production uses the owned path
pub fn convert_rows_with_formats(
    db: Option<DatabaseType>,
    rows: &[Vec<CoreCell>],
    columns: &[ResultColumn],
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Vec<Vec<GridCell>> {
    let plan = column_cell_plan(db, columns, hints, formatting);
    rows.iter()
        .map(|row| convert_row_with_plan(row, &plan))
        .collect()
}

#[must_use]
pub fn convert_rows_owned_with_formats(
    db: Option<DatabaseType>,
    rows: Vec<Vec<CoreCell>>,
    columns: &[ResultColumn],
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Vec<Vec<GridCell>> {
    let plan = column_cell_plan(db, columns, hints, formatting);
    rows.into_iter()
        .map(|row| convert_row_owned_with_plan(row, &plan))
        .collect()
}

#[allow(dead_code)] // borrowed variant retained for tests; production uses the owned path
pub fn build_grid_data_with_formats(
    db: Option<DatabaseType>,
    columns: &[ResultColumn],
    rows: &[Vec<CoreCell>],
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Result<GridData, GridDataError> {
    let grid_columns: Vec<Column> = columns.iter().map(|c| convert_column(db, c)).collect();
    let grid_rows = convert_rows_with_formats(db, rows, columns, hints, formatting);
    GridData::new(grid_columns, grid_rows)
}

pub fn build_grid_data_owned_with_formats(
    db: Option<DatabaseType>,
    columns: &[ResultColumn],
    rows: Vec<Vec<CoreCell>>,
    hints: &[QueryFormatHint],
    formatting: Option<&ResultFormattingPreferences>,
) -> Result<GridData, GridDataError> {
    let grid_columns: Vec<Column> = columns.iter().map(|c| convert_column(db, c)).collect();
    let grid_rows = convert_rows_owned_with_formats(db, rows, columns, hints, formatting);
    GridData::new(grid_columns, grid_rows)
}

/// Convert a grid `CellValue` back to a sqlly-core `CellValue`.
#[must_use]
pub fn grid_cell_to_core(grid: &GridCell) -> CoreCell {
    match grid {
        GridCell::None => CoreCell::Null,
        GridCell::Boolean(b) => CoreCell::Bool(*b),
        GridCell::Integer(i) => CoreCell::I64(*i),
        GridCell::Decimal(f) => CoreCell::F64(*f),
        // Exact decimals came off the wire as canonical text; hand the text
        // back so nothing downstream sees the rounded f64.
        GridCell::ExactDecimal(d) => CoreCell::String(d.as_str().to_owned()),
        GridCell::Text(s) => CoreCell::String(s.clone()),
        GridCell::Date(_) => CoreCell::Null,
    }
}

// ---------------------------------------------------------------------------
// Format / config conversion
// ---------------------------------------------------------------------------

/// Convert the app's `ResultTextAlignment` to the grid's `TextAlignment`.
#[must_use]
pub fn convert_alignment(a: &ResultTextAlignment) -> TextAlignment {
    match a {
        ResultTextAlignment::Left => TextAlignment::Left,
        ResultTextAlignment::Center => TextAlignment::Center,
        ResultTextAlignment::Right => TextAlignment::Right,
    }
}

/// Build a `NumberFormat` for an integer column from the app's
/// `IntegerFormat`.
///
/// Integers group with `,` — the integer settings have no grouping-character
/// choice of their own (the Settings sample groups the same way) — and carry
/// the "Reduce large numbers" metric reduction when it is on.
#[must_use]
pub fn integer_number_format(fmt: &IntegerFormat) -> NumberFormat {
    NumberFormat {
        decimals: 0,
        show_negative_red: fmt.show_red_for_negative,
        negative_parentheses: matches!(fmt.negative_format, NegativeFormatStyle::Parentheses),
        thousands_separator: fmt.use_thousands_separators,
        alignment: convert_alignment(&fmt.alignment),
        thousands_char: ',',
        decimal_char: '.',
        metric: metric_reduction(
            fmt.reduce_large_to_metric,
            fmt.metric_threshold,
            fmt.metric_decimals,
            fmt.metric_step_up_ratio,
            &fmt.metric_labels,
        ),
    }
}

/// Build a `NumberFormat` for a decimal column from the app's
/// `DecimalFormat`: places, grouping on/off and its character, the decimal
/// character, negative style, alignment, and metric reduction.
#[must_use]
pub fn decimal_number_format(fmt: &DecimalFormat) -> NumberFormat {
    let thousands_char = fmt.thousands_separator.literal().chars().next();
    NumberFormat {
        decimals: fmt.decimal_places.max(0) as usize,
        show_negative_red: fmt.show_red_for_negative,
        negative_parentheses: matches!(fmt.negative_format, NegativeFormatStyle::Parentheses),
        // "None" as the grouping character means no grouping at all.
        thousands_separator: fmt.use_thousands_separators && thousands_char.is_some(),
        alignment: convert_alignment(&fmt.alignment),
        thousands_char: thousands_char.unwrap_or(','),
        decimal_char: fmt
            .decimal_separator
            .literal()
            .chars()
            .next()
            .unwrap_or('.'),
        metric: metric_reduction(
            fmt.reduce_large_to_metric,
            fmt.metric_threshold,
            fmt.metric_decimals,
            fmt.metric_step_up_ratio,
            &fmt.metric_labels,
        ),
    }
}

/// The grid's metric reduction for a "Reduce large numbers to K / M / G / T /
/// P" setting, or `None` when it is off.
fn metric_reduction(
    enabled: bool,
    threshold: i64,
    decimals: i32,
    step_up_ratio: f64,
    labels: &crate::preferences::MetricLabels,
) -> Option<MetricReduction> {
    if !enabled {
        return None;
    }
    let ratio = if step_up_ratio.is_finite() {
        step_up_ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    Some(MetricReduction {
        threshold: threshold.max(0) as u64,
        decimals: decimals.max(0) as usize,
        step_up_permille: (ratio * 1000.0).round() as u16,
        labels: [
            MetricLabel::new(&labels.kilo),
            MetricLabel::new(&labels.mega),
            MetricLabel::new(&labels.giga),
            MetricLabel::new(&labels.tera),
            MetricLabel::new(&labels.peta),
        ],
    })
}

/// The grid string format and display rules for a UUID column: the Unique
/// ID alignment, and — with "Show dashes" off — a display-only rule that
/// drops the dashes. The rule runs after formatting, so the cell value (what
/// copy, export, and editing read) keeps the UUID exactly as returned.
fn uuid_display(fmt: &UuidFormat) -> (GridStringFmt, Option<Vec<ReplacementRule>>) {
    let string = GridStringFmt {
        alignment: convert_alignment(&fmt.alignment),
        ..GridStringFmt::default()
    };
    let replacements = (!fmt.show_dashes).then(|| vec![ReplacementRule::new("-", "")]);
    (string, replacements)
}

/// Build a `BooleanFormat` from the app's `BoolFormat`.
#[must_use]
pub fn boolean_format(
    style: &BoolFormatStyle,
    custom_true: &str,
    custom_false: &str,
    alignment: &ResultTextAlignment,
) -> GridBoolFmt {
    let (true_text, false_text) = match style {
        BoolFormatStyle::YN => ("y".to_string(), "n".to_string()),
        BoolFormatStyle::YesNo => ("yes".to_string(), "no".to_string()),
        BoolFormatStyle::ZeroOne => ("1".to_string(), "0".to_string()),
        BoolFormatStyle::TF => ("t".to_string(), "f".to_string()),
        BoolFormatStyle::TrueFalse => ("true".to_string(), "false".to_string()),
        BoolFormatStyle::Custom => (custom_true.to_string(), custom_false.to_string()),
    };
    GridBoolFmt {
        true_text,
        false_text,
        alignment: convert_alignment(alignment),
    }
}

/// Build a `DateFormat` for the grid from the app's `DateFormat`.
///
/// The app uses a Swift-style format string (e.g. `"yyyy-MM-dd HH:mm:ss"`);
/// the grid uses `strftime`-style (e.g. `"%Y-%m-%d %H:%M:%S"`). This function
/// translates the common tokens.
#[must_use]
pub fn convert_date_format(fmt: &DateFormat) -> GridDateFmt {
    let format = crate::result_cell_format::swift_date_format_to_strftime(&fmt.format);
    GridDateFmt {
        format,
        timezone_offset_minutes: 0,
        relative: None,
        alignment: convert_alignment(&fmt.alignment),
    }
}

/// Build a `GridConfig` from the app's `ResultFormattingPreferences` and the
/// result set's column metadata. Per-column overrides are resolved against
/// the column's inferred kind and the app's `column_overrides` map (keyed by
/// column name).
#[must_use]
pub fn build_grid_config(
    db: Option<DatabaseType>,
    prefs: &ResultFormattingPreferences,
    columns: &[ResultColumn],
) -> GridConfig {
    let default_number = decimal_number_format(&prefs.defaults.decimal_format);
    let default_date = convert_date_format(&prefs.defaults.date_format);
    let default_boolean = boolean_format(
        &prefs.defaults.bool_format.style,
        &prefs.defaults.bool_format.custom_true,
        &prefs.defaults.bool_format.custom_false,
        &prefs.defaults.bool_format.alignment,
    );
    let default_string = GridStringFmt::default();

    let column_overrides: Vec<ColumnOverride> = columns
        .iter()
        .map(|col| {
            let inferred = infer_result_kind_for(db, Some(col));
            let override_fmt = prefs.column_overrides.get(&col.name);
            build_column_override(inferred, override_fmt, &prefs.defaults)
        })
        .collect();

    let mut config = GridConfig {
        default_number,
        default_date,
        default_boolean,
        default_string,
        default_null: NullFormat {
            text: "NULL".into(),
            italic: true,
            background: false,
        },
        ..GridConfig::default()
    }
    .with_column_overrides(column_overrides);
    // Conditional formatting rules are global by column name; the grid
    // resolves them against its own columns (unknown names are ignored).
    config.conditional_rules = crate::conditional_formatting::load_rules();
    // "Color cells by data type" appends one catch-all tint per column AFTER
    // the user's own rules, so a user rule for the same column still wins
    // (first matching rule takes the cell) and the tints only fill the gaps.
    if prefs.color_cells_by_type {
        append_type_color_rules(
            &mut config,
            db,
            columns,
            &prefs.column_overrides,
            &prefs.type_colors,
        );
    }
    // "Syntax-color structured values in cells": JSON/XML token colors from
    // the theme, painted by the grid over each cell's visible prefix.
    if prefs.syntax_color_structured {
        config.cell_text_styler = crate::cell_syntax::StructuredCellStyler::for_columns(
            db,
            columns,
            crate::cell_syntax::SyntaxPalette::from_theme(),
        )
        .map(sqlly_datatable::CellStyler::new);
    }
    // Motion (fades, dialog/scrim transitions, macOS scroll physics) follows
    // the Results > General preference; the data surface stays instant either
    // way. Default-on, so only an explicit opt-out turns it off.
    config.animations = !prefs.disable_animations;
    config
}

/// Append one catch-all background-tint rule per column to `config`, for the
/// "Color cells by data type" preference. Tints follow the active theme, so
/// they work in every theme family, unless the user picked their own fill or
/// text color for the kind (`colors`). A column is left untinted when:
///
/// * it is masked (an explicit or auto-mask override), so sensitive data
///   does not advertise itself with a color;
/// * the user already gave it a color scale — the grid lets any matching
///   rule's background beat a color scale regardless of order, so a tint
///   would silently hide the user's heatmap;
/// * several result columns share its name (case-insensitively, as the grid
///   matches rules) with different tints — one name-keyed rule would paint
///   every one of them in the first column's color.
fn append_type_color_rules(
    config: &mut GridConfig,
    db: Option<DatabaseType>,
    columns: &[ResultColumn],
    overrides: &std::collections::BTreeMap<String, crate::preferences::ColumnFormat>,
    colors: &std::collections::BTreeMap<TypeColorKind, TypeColorStyle>,
) {
    let has_user_scale = |name: &str| {
        config.conditional_rules.iter().any(|rule| {
            rule.column.eq_ignore_ascii_case(name)
                && matches!(rule.kind, ConditionalKind::ColorScale { .. })
        })
    };
    let mut styles: Vec<(String, Option<ConditionalStyle>)> = Vec::new();
    for col in columns {
        let style = type_color_kind(db, col, overrides.get(&col.name))
            .map(|kind| type_color_style(kind, colors.get(&kind)));
        match styles
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&col.name))
        {
            Some((_, existing)) if *existing != style => *existing = None,
            Some(_) => {}
            None => styles.push((col.name.clone(), style)),
        }
    }
    let rules: Vec<ConditionalRule> = styles
        .into_iter()
        .filter(|(name, _)| !has_user_scale(name))
        .filter_map(|(name, style)| style.map(|style| type_color_rule(name, style)))
        .collect();
    config.conditional_rules.extend(rules);
}

/// The catch-all rule "Color cells by data type" adds for one column.
fn type_color_rule(column: String, style: ConditionalStyle) -> ConditionalRule {
    ConditionalRule {
        column,
        kind: ConditionalKind::Rule {
            condition: ConditionalCondition::IsNotNull,
            style,
        },
    }
}

/// Whether `rule` has the shape "Color cells by data type" writes: a
/// non-null catch-all with only a fill and/or text color.
fn is_type_color_rule(rule: &ConditionalRule) -> bool {
    matches!(
        &rule.kind,
        ConditionalKind::Rule {
            condition: ConditionalCondition::IsNotNull,
            style,
        } if !style.bold && (style.background.is_some() || style.foreground.is_some())
    )
}

/// Drop the "Color cells by data type" tint from `column` — for masks that
/// arrive after the config is built (a `-- sqlly format: …; mask=…`
/// directive), which must stay as plain as any other mask.
pub fn drop_type_color_rule(config: &mut GridConfig, column: &str) {
    config
        .conditional_rules
        .retain(|rule| !(rule.column.eq_ignore_ascii_case(column) && is_type_color_rule(rule)));
}

/// The kind a column colors as under "Color cells by data type", keyed off
/// the column's effective kind (a user override, else the same type
/// inference formatting uses), plus the binary family (which formats as
/// text but colors as its own kind). `None` means "no color" — masked
/// columns stay plain.
fn type_color_kind(
    db: Option<DatabaseType>,
    col: &ResultColumn,
    override_fmt: Option<&crate::preferences::ColumnFormat>,
) -> Option<TypeColorKind> {
    let kind = match override_fmt {
        Some(fmt) => fmt.kind,
        None if matches!(type_family(db, &col.data_type), Some(TypeFamily::Binary)) => {
            return Some(TypeColorKind::Binary);
        }
        None => infer_result_kind_for(db, Some(col)),
    };
    match kind {
        ResultKind::Integer | ResultKind::Decimal | ResultKind::Number | ResultKind::Epoch => {
            Some(TypeColorKind::Number)
        }
        ResultKind::Date => Some(TypeColorKind::Date),
        ResultKind::Bool => Some(TypeColorKind::Boolean),
        ResultKind::Uuid => Some(TypeColorKind::Uuid),
        ResultKind::Text | ResultKind::JsonPath | ResultKind::Template => Some(TypeColorKind::Text),
        ResultKind::Mask => None,
    }
}

/// The theme's background tint for a data kind.
pub fn theme_type_tint(kind: TypeColorKind) -> RgbaColor {
    match kind {
        TypeColorKind::Number => themed_tint(Theme::info(), 0.14),
        TypeColorKind::Date => themed_tint(Theme::love(), 0.12),
        TypeColorKind::Boolean => themed_tint(Theme::success(), 0.12),
        TypeColorKind::Uuid => themed_tint(Theme::badge_fn(), 0.12),
        TypeColorKind::Text => themed_tint(Theme::accent(), 0.05),
        TypeColorKind::Binary => themed_tint(Theme::warning(), 0.12),
    }
}

/// The style a kind's cells get: the user's fill (else the theme tint) and
/// the user's text color, if any.
fn type_color_style(kind: TypeColorKind, custom: Option<&TypeColorStyle>) -> ConditionalStyle {
    ConditionalStyle {
        background: Some(
            custom
                .and_then(|c| c.background)
                .unwrap_or_else(|| theme_type_tint(kind)),
        ),
        foreground: custom.and_then(|c| c.foreground),
        bold: false,
    }
}

/// The theme background tint a column gets under "Color cells by data
/// type" (see [`type_color_kind`]).
#[cfg(test)]
fn type_color_tint(
    db: Option<DatabaseType>,
    col: &ResultColumn,
    override_fmt: Option<&crate::preferences::ColumnFormat>,
) -> Option<RgbaColor> {
    type_color_kind(db, col, override_fmt).map(theme_type_tint)
}

/// A theme color at a low alpha, in the serde-friendly form conditional rules
/// carry. The grid paints it as a translucent fill over the cell, so the same
/// rule reads correctly on the light and dark families without per-theme
/// constants.
fn themed_tint(color: gpui::Hsla, alpha: f32) -> RgbaColor {
    let rgba: gpui::Rgba = gpui::hsla(color.h, color.s, color.l, alpha).into();
    RgbaColor::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

/// Build a per-column `ColumnOverride` from the inferred kind and an optional
/// app-level per-column override.
fn build_column_override(
    inferred: ResultKind,
    override_fmt: Option<&crate::preferences::ColumnFormat>,
    defaults: &crate::preferences::ResultTypeDefaults,
) -> ColumnOverride {
    // The effective kind: override wins when not "inherit".
    let kind = override_fmt
        .filter(|o| !o.is_inherit())
        .map(|o| o.kind)
        .unwrap_or(inferred);

    let int_fmt = override_fmt
        .map(|o| &o.integer_format)
        .unwrap_or(&defaults.integer_format);
    let dec_fmt = override_fmt
        .map(|o| &o.decimal_format)
        .unwrap_or(&defaults.decimal_format);
    let bool_fmt = override_fmt
        .map(|o| &o.bool_format)
        .unwrap_or(&defaults.bool_format);
    let date_fmt = override_fmt
        .map(|o| &o.date_format)
        .unwrap_or(&defaults.date_format);
    let uuid_fmt = override_fmt
        .map(|o| &o.uuid_format)
        .unwrap_or(&defaults.uuid_format);

    let number = match kind {
        ResultKind::Integer => Some(integer_number_format(int_fmt)),
        ResultKind::Decimal => Some(decimal_number_format(dec_fmt)),
        _ => None,
    };
    let date = match kind {
        ResultKind::Date => Some(convert_date_format(date_fmt)),
        _ => None,
    };
    let boolean = match kind {
        ResultKind::Bool => Some(boolean_format(
            &bool_fmt.style,
            &bool_fmt.custom_true,
            &bool_fmt.custom_false,
            &bool_fmt.alignment,
        )),
        _ => None,
    };

    let (string, replacements) = match kind {
        ResultKind::Uuid => {
            let (string, replacements) = uuid_display(uuid_fmt);
            (Some(string), replacements)
        }
        _ => (None, None),
    };

    ColumnOverride {
        number,
        date,
        boolean,
        string,
        null: None,
        replacements,
        replacement_timing: replacements_timing(kind),
        value_labels: None,
        label_mode: None,
        header_tooltip: None,
    }
}

/// UUID dash stripping must run on the formatted text (after case/length
/// shaping), never on the stored value.
fn replacements_timing(kind: ResultKind) -> Option<ReplacementTiming> {
    (kind == ResultKind::Uuid).then_some(ReplacementTiming::AfterFormat)
}

/// Attach source-column provenance tooltips to the result grid's headers:
/// hovering a header shows where the column's values really come from, e.g.
/// "Source: p.Name from Shared.Permission p" for `SELECT p.Name AS
/// PermissionName …`. Columns without a certain single source (computed
/// expressions, ambiguous stars, multi-statement scripts) get no tooltip.
pub fn apply_provenance_tooltips(
    config: &mut GridConfig,
    columns: &[ResultColumn],
    sql: Option<&str>,
) {
    use sqlly_intel::query::column_provenance::StatementProvenance;

    let Some(provenance) = sql.and_then(StatementProvenance::parse) else {
        return;
    };
    // Label occurrence counts from the actual result set, so star expansion
    // cannot be confused with an identically named explicit projection.
    let mut occurrences: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for col in columns {
        *occurrences
            .entry(col.name.to_ascii_lowercase())
            .or_default() += 1;
    }
    if config.column_overrides.len() < columns.len() {
        config
            .column_overrides
            .resize(columns.len(), ColumnOverride::default());
    }
    for (index, col) in columns.iter().enumerate() {
        let count = occurrences
            .get(&col.name.to_ascii_lowercase())
            .copied()
            .unwrap_or(1);
        let Some(source) = provenance.resolve(&col.name, index, count) else {
            continue;
        };
        let origin = provenance.resolve_origin(&col.name, index, count);
        config.column_overrides[index].header_tooltip =
            Some(provenance_tooltip_text(&source, origin.as_ref()));
    }
}

/// The source table and column behind each result column, for matching
/// qualified `table.column` Column overrides. Uses the executed statement's
/// provenance (the same attribution the header tooltips show); when the
/// statement cannot be attributed at all but reads a single table, every
/// column is taken to come from that table. Columns without a certain
/// source get `None`.
pub fn column_sources(
    columns: &[ResultColumn],
    sql: Option<&str>,
) -> Vec<Option<crate::result_cell_format::ColumnSource>> {
    use crate::result_cell_format::ColumnSource;
    use sqlly_intel::query::column_provenance::StatementProvenance;

    let Some(sql) = sql else {
        return vec![None; columns.len()];
    };
    let Some(provenance) = StatementProvenance::parse(sql) else {
        let Some((schema, table)) = crate::column_layout::single_table_target(sql) else {
            return vec![None; columns.len()];
        };
        let chain: Vec<String> = schema.into_iter().chain(std::iter::once(table)).collect();
        return columns
            .iter()
            .map(|col| {
                Some(ColumnSource {
                    table: chain.clone(),
                    column: col.name.clone(),
                })
            })
            .collect();
    };
    let mut occurrences: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for col in columns {
        *occurrences
            .entry(col.name.to_ascii_lowercase())
            .or_default() += 1;
    }
    columns
        .iter()
        .enumerate()
        .map(|(index, col)| {
            let count = occurrences
                .get(&col.name.to_ascii_lowercase())
                .copied()
                .unwrap_or(1);
            provenance
                .resolve_origin(&col.name, index, count)
                .map(|origin| ColumnSource {
                    table: origin.table,
                    column: origin.column,
                })
        })
        .collect()
}

/// The header-tooltip text for a resolved column: the outer-scope source
/// reference, plus the traced base table when it adds information.
fn provenance_tooltip_text(
    source: &sqlly_intel::query::column_provenance::SourceColumn,
    origin: Option<&sqlly_intel::query::column_provenance::ColumnOrigin>,
) -> String {
    match origin {
        Some(origin) => {
            let table = origin.table.join(".");
            match &origin.alias {
                Some(alias) => format!("Source: {alias}.{} from {table} {alias}", origin.column),
                None => format!("Source: {table}.{}", origin.column),
            }
        }
        None if source.qualifier.is_empty() => format!("Source: {}", source.column),
        None => format!("Source: {}.{}", source.qualifier.join("."), source.column),
    }
}

/// Attach cached FK display labels to the resolved column overrides.
pub fn apply_fk_value_labels(
    config: &mut GridConfig,
    fks: &[crate::fk_labels::LabelledFk],
    rows: &[Vec<sqlly_core::CellValue>],
    cache: &mut crate::fk_labels::LabelCache,
    mode: LabelMode,
) {
    use std::sync::Arc;
    for fk in fks {
        let labels = crate::fk_labels::value_labels_for(fk, rows, cache);
        if labels.is_empty() {
            continue;
        }
        if let Some(override_) = config.column_overrides.get_mut(fk.column_index) {
            override_.value_labels = Some(Arc::new(labels));
            override_.label_mode = Some(mode);
        }
    }
}

// ---------------------------------------------------------------------------
// Theme mapping
// ---------------------------------------------------------------------------

/// Build a `GridTheme` bridged from the app `Theme` tokens.
#[must_use]
pub fn build_grid_theme() -> GridTheme {
    bridged_grid_theme()
}

/// Mapping from the app `Theme` colors to the result grid.
fn bridged_grid_theme() -> GridTheme {
    GridTheme {
        bg: Theme::bg(),
        header_bg: Theme::panel_bg(),
        filter_bg: Theme::panel_bg(),
        filter_active_bg: Theme::selection_fill(),
        row_header_bg: Theme::panel_bg(),
        selection_bg: Theme::selection_fill(),
        alt_row_bg: Theme::row_alt(),
        grid_line: Theme::border(),
        header_fg: Theme::text(),
        text_fg: Theme::text(),
        negative_fg: Theme::error(),
        sort_indicator: Theme::accent(),
        filter_cursor: Theme::cursor(),
        menu_bg: Theme::popup_bg(),
        menu_hover_bg: Theme::completion_selected(),
        menu_fg: Theme::text(),
        muted_text: Theme::text_dim(),
        null_fg: Theme::text_dim(),
        null_bg: Theme::sidebar_bg(),
        pivot_group_bg: Theme::sidebar_bg(),
        pivot_subtotal_bg: Theme::row_alt(),
        pivot_grand_total_bg: Theme::selection_fill(),
        pivot_total_fg: Theme::text(),
        pivot_drop_zone_bg: Theme::panel_bg(),
        pivot_drop_zone_active_bg: Theme::selection_fill(),
        pivot_chip_bg: Theme::completion_selected(),
        pivot_chip_fg: Theme::text(),
        // Matches the app chrome's own scrollbar thumb (see `theme.rs`).
        scrollbar_thumb: Theme::text(),
        // No app-chrome equivalent; mirrors the crate's shipped scrims,
        // which go heavier on dark so the dialog still reads as lifted.
        overlay_scrim: gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: if matches!(crate::theme::resolved_kind(), crate::theme::ThemeKind::Dark) {
                0.45
            } else {
                0.35
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Extension trait for GridConfig builder ergonomics
// ---------------------------------------------------------------------------

/// Private extension to set column overrides in a builder pattern.
trait GridConfigExt {
    fn with_column_overrides(self, overrides: Vec<ColumnOverride>) -> Self;
}

impl GridConfigExt for GridConfig {
    fn with_column_overrides(mut self, overrides: Vec<ColumnOverride>) -> Self {
        self.column_overrides = overrides;
        self
    }
}

// ---------------------------------------------------------------------------
// Staged-edit marker column (plan 164)
// ---------------------------------------------------------------------------

/// Leading marker column name — a single space so it stays visually minimal.
pub const MARKER_COLUMN_NAME: &str = " ";

/// Width of the staged-change marker column in logical pixels.
pub const MARKER_COLUMN_WIDTH: f32 = 18.0;

/// Low-alpha tint used for staged-row marker backgrounds.
pub const MARKER_TINT_ALPHA: f32 = 0.18;

#[must_use]
pub fn marker_column() -> Column {
    Column {
        name: MARKER_COLUMN_NAME.to_string(),
        kind: ColumnKind::Text,
        width: MARKER_COLUMN_WIDTH,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_column_has_expected_width_and_name() {
        let col = marker_column();
        assert_eq!(col.name, MARKER_COLUMN_NAME);
        assert_eq!(col.width, MARKER_COLUMN_WIDTH);
        assert_eq!(col.kind, ColumnKind::Text);
    }

    #[test]
    fn convert_cell_null() {
        assert_eq!(convert_cell(&CoreCell::Null), GridCell::None);
    }

    #[test]
    fn grid_animations_follow_preference() {
        use crate::preferences::ResultFormattingPreferences;
        // Default preference (animations enabled) → grid animates.
        let on = ResultFormattingPreferences::default();
        assert!(build_grid_config(None, &on, &[]).animations);
        // Explicit opt-out disables grid (and, downstream, pivot) motion.
        let off = ResultFormattingPreferences {
            disable_animations: true,
            ..Default::default()
        };
        assert!(!build_grid_config(None, &off, &[]).animations);
    }

    #[test]
    fn type_colors_off_by_default_adds_no_rules() {
        use crate::preferences::ResultFormattingPreferences;
        let prefs = ResultFormattingPreferences::default();
        assert!(!prefs.color_cells_by_type);
        let col = ResultColumn {
            name: "n".into(),
            data_type: "int".into(),
            nullable: true,
        };
        // Isolated test HOME → no user rules on disk, and the opt-in is off.
        let config = build_grid_config(None, &prefs, std::slice::from_ref(&col));
        assert!(config.conditional_rules.is_empty());
    }

    #[test]
    fn type_colors_enabled_appends_one_catch_all_tint_per_column() {
        use crate::preferences::ResultFormattingPreferences;
        let prefs = ResultFormattingPreferences {
            color_cells_by_type: true,
            ..Default::default()
        };
        let cols = [
            ("id", "int"),
            ("name", "nvarchar"),
            ("created", "datetime"),
            ("active", "bit"),
            ("stamp", "rowversion"),
        ]
        .map(|(name, data_type)| ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        });
        let config = build_grid_config(Some(DatabaseType::SqlServer), &prefs, &cols);
        assert_eq!(config.conditional_rules.len(), cols.len());
        for (rule, col) in config.conditional_rules.iter().zip(&cols) {
            assert_eq!(rule.column, col.name);
            let ConditionalKind::Rule { condition, style } = &rule.kind else {
                panic!("type colors must be plain rules");
            };
            assert!(matches!(condition, ConditionalCondition::IsNotNull));
            assert!(style.background.is_some());
            assert!(style.foreground.is_none());
            assert!(!style.bold);
        }
    }

    #[test]
    fn type_color_tints_differ_by_kind_and_binary_is_its_own() {
        let db = Some(DatabaseType::SqlServer);
        let tint_of = |data_type: &str| {
            let col = ResultColumn {
                name: "c".into(),
                data_type: data_type.into(),
                nullable: true,
            };
            type_color_tint(db, &col, None).expect("every non-mask kind gets a tint")
        };
        // Integer and decimal are one numeric family and share a tint.
        assert_eq!(tint_of("int"), tint_of("decimal"));
        let tints = [
            tint_of("int"),
            tint_of("datetime"),
            tint_of("bit"),
            tint_of("nvarchar"),
            tint_of("uniqueidentifier"),
            tint_of("varbinary"),
        ];
        for (i, a) in tints.iter().enumerate() {
            for b in &tints[i + 1..] {
                assert_ne!(a, b, "tints must differ per data-type family");
            }
        }
    }

    #[test]
    fn type_color_rules_are_appended_after_user_rules() {
        let mut config = GridConfig::default();
        config.conditional_rules.push(ConditionalRule {
            column: "id".into(),
            kind: ConditionalKind::Rule {
                condition: ConditionalCondition::IsNull,
                style: ConditionalStyle {
                    foreground: Some(RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                    ..ConditionalStyle::default()
                },
            },
        });
        let col = ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: true,
        };
        append_type_color_rules(
            &mut config,
            None,
            std::slice::from_ref(&col),
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(config.conditional_rules.len(), 2);
        // The user's rule stays first; the grid's first-matching-rule-wins
        // evaluation therefore lets it override the type tint.
        assert!(matches!(
            config.conditional_rules[0].kind,
            ConditionalKind::Rule {
                condition: ConditionalCondition::IsNull,
                ..
            }
        ));
        assert!(matches!(
            config.conditional_rules[1].kind,
            ConditionalKind::Rule {
                condition: ConditionalCondition::IsNotNull,
                ..
            }
        ));
    }

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    #[test]
    fn epoch_columns_follow_the_global_display_zone() {
        use crate::preferences::{
            ColumnFormat, DateTimeZoneMode, EpochFormat, ResultFormattingPreferences,
        };
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.date_format.time_zone_mode = DateTimeZoneMode::AlwaysInTimeZone;
        prefs.defaults.date_format.time_zone_identifier = "Asia/Tokyo".into();
        prefs.column_overrides.insert(
            "created".into(),
            ColumnFormat::default_for_kind(ResultKind::Epoch),
        );
        let mut pinned = ColumnFormat::default_for_kind(ResultKind::Epoch);
        pinned.epoch_format = EpochFormat {
            time_zone_mode: Some(DateTimeZoneMode::AlwaysInTimeZone),
            time_zone_identifier: "UTC".into(),
            ..EpochFormat::default()
        };
        prefs.column_overrides.insert("pinned".into(), pinned);
        let cols = [col("created", "bigint"), col("pinned", "bigint")];
        // 2026-01-01T12:00:00Z → 21:00 in Tokyo.
        let row = [CoreCell::I64(1_767_268_800), CoreCell::I64(1_767_268_800)];
        let cells =
            convert_row_with_formats(Some(DatabaseType::Postgres), &row, &cols, &[], Some(&prefs));
        assert_eq!(cells[0], GridCell::Text("2026-01-01 21:00:00".into()));
        assert_eq!(
            cells[1],
            GridCell::Text("2026-01-01 12:00:00".into()),
            "an explicit zone on the override still wins"
        );
    }

    #[test]
    fn date_overrides_follow_the_global_zone_and_pins_or_directives_win() {
        use crate::preferences::{
            ColumnFormat, ColumnZoneChoice, DateTimeZoneMode, ResultFormattingPreferences,
        };
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.date_format.time_zone_mode = DateTimeZoneMode::AlwaysInTimeZone;
        prefs.defaults.date_format.time_zone_identifier = "Asia/Tokyo".into();
        prefs.column_overrides.insert(
            "follows".into(),
            ColumnFormat::default_for_kind(ResultKind::Date),
        );
        let mut pinned = ColumnFormat::default_for_kind(ResultKind::Date);
        pinned.date_format.set_zone_choice(ColumnZoneChoice::Utc);
        prefs.column_overrides.insert("pinned".into(), pinned);
        let cols = [
            col("follows", "timestamptz"),
            col("pinned", "timestamptz"),
            col("directed", "timestamptz"),
        ];
        let hints = crate::query_format::parse(
            "-- sqlly format: column=pinned; zone=Europe/Berlin\n\
             -- sqlly format: column=directed; date=%H:%M; zone=UTC",
        );
        let instant = CoreCell::String("2026-01-01T12:00:00+00:00".into());
        let row = [instant.clone(), instant.clone(), instant];
        let cells =
            convert_row_with_formats(Some(DatabaseType::Postgres), &row, &cols, &[], Some(&prefs));
        assert_eq!(cells[0], GridCell::Text("2026-01-01 21:00:00".into()));
        assert_eq!(cells[1], GridCell::Text("2026-01-01 12:00:00".into()));
        let cells = convert_row_with_formats(
            Some(DatabaseType::Postgres),
            &row,
            &cols,
            &hints,
            Some(&prefs),
        );
        assert_eq!(
            cells[1],
            GridCell::Text("2026-01-01 13:00:00".into()),
            "a zone= directive beats the override's pinned zone"
        );
        assert_eq!(cells[2], GridCell::Text("12:00".into()));
    }

    #[test]
    fn connection_zone_drives_following_columns_but_not_pinned_ones() {
        use crate::preferences::{
            ColumnFormat, ColumnZoneChoice, EpochFormat, ResultFormattingPreferences,
        };
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "created".into(),
            ColumnFormat::default_for_kind(ResultKind::Epoch),
        );
        let mut pinned = ColumnFormat::default_for_kind(ResultKind::Epoch);
        let mut epoch = EpochFormat::default();
        epoch.set_zone_choice(ColumnZoneChoice::Utc);
        pinned.epoch_format = epoch;
        prefs.column_overrides.insert("pinned".into(), pinned);
        crate::result_cell_format::apply_connection_display_zone(
            &mut prefs,
            Some(&sqlly_core::DisplayTimeZone::Named("Asia/Tokyo".into())),
        );
        let cols = [
            col("created", "bigint"),
            col("pinned", "bigint"),
            col("seen", "timestamptz"),
        ];
        let row = [
            CoreCell::I64(1_767_268_800),
            CoreCell::I64(1_767_268_800),
            CoreCell::String("2026-01-01T12:00:00+00:00".into()),
        ];
        let cells =
            convert_row_with_formats(Some(DatabaseType::Postgres), &row, &cols, &[], Some(&prefs));
        assert_eq!(cells[0], GridCell::Text("2026-01-01 21:00:00".into()));
        assert_eq!(cells[1], GridCell::Text("2026-01-01 12:00:00".into()));
        assert_eq!(cells[2], GridCell::Text("2026-01-01 21:00:00".into()));
    }

    #[test]
    fn integer_timestamps_are_detected_only_when_asked() {
        use crate::preferences::{ColumnFormat, EpochAutoDetect, ResultFormattingPreferences};
        let cols = [
            col("created_at", "bigint"),
            col("response_time", "int"),
            col("updated_at", "bigint"),
            col("expires_at", "bigint"),
            col("label_at", "varchar"),
        ];
        // 2026-01-01T12:00:00Z in seconds and in milliseconds.
        let row = [
            CoreCell::I64(1_767_268_800),
            CoreCell::I64(1_767_268_800),
            CoreCell::I64(1_767_268_800_000),
            CoreCell::I64(3600),
            CoreCell::String("1767268800".into()),
        ];
        let db = Some(DatabaseType::Postgres);
        let mut prefs = ResultFormattingPreferences::default();
        let off = convert_row_with_formats(db, &row, &cols, &[], Some(&prefs));
        assert_eq!(off[0], GridCell::Integer(1_767_268_800), "off by default");

        prefs.epoch_auto_detect = EpochAutoDetect::ByName;
        let by_name = convert_row_with_formats(db, &row, &cols, &[], Some(&prefs));
        assert_eq!(by_name[0], GridCell::Text("2026-01-01 12:00:00".into()));
        assert_eq!(by_name[1], GridCell::Integer(1_767_268_800), "a duration");
        assert_eq!(by_name[2], GridCell::Text("2026-01-01 12:00:00".into()));
        assert_eq!(by_name[3], GridCell::Text("1970-01-01 01:00:00".into()));
        assert_eq!(
            by_name[4],
            GridCell::Text("1767268800".into()),
            "text columns are never guessed"
        );

        prefs.epoch_auto_detect = EpochAutoDetect::ByNameAndRange;
        let ranged = convert_row_with_formats(db, &row, &cols, &[], Some(&prefs));
        assert_eq!(ranged[0], GridCell::Text("2026-01-01 12:00:00".into()));
        assert_eq!(
            ranged[3],
            GridCell::Integer(3600),
            "out of range stays a number"
        );

        // An explicit override (or directive) on the column wins.
        prefs.column_overrides.insert(
            "created_at".into(),
            ColumnFormat::default_for_kind(ResultKind::Integer),
        );
        let hints = crate::query_format::parse("-- sqlly format: column=updated_at; number=0");
        let explicit = convert_row_with_formats(db, &row, &cols, &hints, Some(&prefs));
        assert_eq!(explicit[0], GridCell::Integer(1_767_268_800));
        assert_eq!(explicit[2], GridCell::Integer(1_767_268_800_000));
    }

    #[test]
    fn type_colors_skip_masked_columns_and_follow_overrides() {
        use crate::preferences::{ColumnFormat, ResultFormattingPreferences};
        let mut prefs = ResultFormattingPreferences {
            color_cells_by_type: true,
            ..Default::default()
        };
        prefs.column_overrides.insert(
            "ssn".into(),
            ColumnFormat::default_for_kind(ResultKind::Mask),
        );
        prefs.column_overrides.insert(
            "code".into(),
            ColumnFormat::default_for_kind(ResultKind::Date),
        );
        let cols = [col("ssn", "nvarchar"), col("code", "nvarchar")];
        let config = build_grid_config(Some(DatabaseType::SqlServer), &prefs, &cols);
        let names: Vec<&str> = config
            .conditional_rules
            .iter()
            .map(|r| r.column.as_str())
            .collect();
        assert_eq!(names, ["code"], "a masked column is never tinted");
        let date_tint = type_color_tint(Some(DatabaseType::SqlServer), &col("x", "datetime"), None);
        let ConditionalKind::Rule { style, .. } = &config.conditional_rules[0].kind else {
            panic!("plain rule");
        };
        assert_eq!(
            style.background, date_tint,
            "the override's kind picks the tint"
        );
    }

    #[test]
    fn syntax_coloring_is_opt_in_and_skips_non_text_columns() {
        use crate::preferences::ResultFormattingPreferences;
        let cols = [col("doc", "jsonb"), col("n", "int")];
        let db = Some(DatabaseType::Postgres);
        let mut prefs = ResultFormattingPreferences::default();
        assert!(
            build_grid_config(db, &prefs, &cols)
                .cell_text_styler
                .is_none()
        );
        prefs.syntax_color_structured = true;
        let styler = build_grid_config(db, &prefs, &cols)
            .cell_text_styler
            .expect("a JSON column gets a styler");
        assert!(styler.0.styles_column("doc"));
        assert!(!styler.0.styles_column("n"));
        assert!(
            build_grid_config(db, &prefs, &[col("n", "int")])
                .cell_text_styler
                .is_none()
        );
    }

    #[test]
    fn custom_type_colors_replace_the_theme_tint_per_kind() {
        use crate::preferences::{ResultFormattingPreferences, TypeColorKind, TypeColorStyle};
        let red = RgbaColor::new(1.0, 0.0, 0.0, 0.3);
        let blue = RgbaColor::new(0.0, 0.0, 1.0, 1.0);
        let mut prefs = ResultFormattingPreferences {
            color_cells_by_type: true,
            ..ResultFormattingPreferences::default()
        };
        prefs.type_colors.insert(
            TypeColorKind::Number,
            TypeColorStyle {
                background: Some(red),
                foreground: Some(blue),
            },
        );
        // A text color alone keeps the theme fill.
        prefs.type_colors.insert(
            TypeColorKind::Date,
            TypeColorStyle {
                background: None,
                foreground: Some(blue),
            },
        );
        let cols = [col("n", "int"), col("d", "datetime"), col("s", "nvarchar")];
        let config = build_grid_config(Some(DatabaseType::SqlServer), &prefs, &cols);
        let style_of = |name: &str| {
            config
                .conditional_rules
                .iter()
                .find(|r| r.column == name)
                .and_then(|r| match &r.kind {
                    ConditionalKind::Rule { style, .. } => Some(*style),
                    _ => None,
                })
                .expect("a type color rule")
        };
        assert_eq!(style_of("n").background, Some(red));
        assert_eq!(style_of("n").foreground, Some(blue));
        // Theme tints (not compared exactly: other tests switch the global
        // theme concurrently) where no fill was picked.
        let date_fill = style_of("d").background.expect("theme fill");
        assert_ne!(date_fill, red);
        assert_eq!(style_of("d").foreground, Some(blue));
        let text_fill = style_of("s").background.expect("theme fill");
        assert_ne!(text_fill, red);
        assert_eq!(style_of("s").foreground, None);
        // A directive mask arriving later still strips a custom-colored rule.
        let mut config = config;
        drop_type_color_rule(&mut config, "N");
        assert!(config.conditional_rules.iter().all(|r| r.column != "n"));
        // Stored preferences round-trip, and an empty map is not written.
        let json = serde_json::to_string(&prefs).unwrap();
        let back: ResultFormattingPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.type_colors, prefs.type_colors);
        let plain = serde_json::to_string(&ResultFormattingPreferences::default()).unwrap();
        assert!(!plain.contains("typeColors"));
    }

    #[test]
    fn type_colors_never_hide_a_user_color_scale() {
        let mut config = GridConfig::default();
        config.conditional_rules.push(ConditionalRule {
            column: "Total".into(),
            kind: ConditionalKind::ColorScale {
                min: RgbaColor::new(1.0, 1.0, 1.0, 1.0),
                max: RgbaColor::new(0.0, 0.5, 0.0, 1.0),
            },
        });
        let cols = [col("total", "int"), col("qty", "int")];
        append_type_color_rules(
            &mut config,
            None,
            &cols,
            &Default::default(),
            &Default::default(),
        );
        let tinted: Vec<&str> = config
            .conditional_rules
            .iter()
            .filter(|r| matches!(r.kind, ConditionalKind::Rule { .. }))
            .map(|r| r.column.as_str())
            .collect();
        assert_eq!(tinted, ["qty"]);
    }

    #[test]
    fn type_colors_skip_duplicate_names_of_different_kinds() {
        let mut config = GridConfig::default();
        let cols = [
            col("id", "int"),
            col("ID", "uniqueidentifier"),
            col("n", "int"),
            col("N", "bigint"),
        ];
        append_type_color_rules(
            &mut config,
            Some(DatabaseType::SqlServer),
            &cols,
            &Default::default(),
            &Default::default(),
        );
        let names: Vec<&str> = config
            .conditional_rules
            .iter()
            .map(|r| r.column.as_str())
            .collect();
        // `id`/`ID` disagree, so neither is tinted; `n`/`N` agree and get
        // one shared rule.
        assert_eq!(names, ["n"]);
    }

    #[test]
    fn drop_type_color_rule_leaves_user_rules_alone() {
        let mut config = GridConfig::default();
        config.conditional_rules.push(ConditionalRule {
            column: "secret".into(),
            kind: ConditionalKind::Rule {
                condition: ConditionalCondition::IsNotNull,
                style: ConditionalStyle {
                    background: Some(RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                    bold: true,
                    ..ConditionalStyle::default()
                },
            },
        });
        append_type_color_rules(
            &mut config,
            None,
            &[col("secret", "int")],
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(config.conditional_rules.len(), 2);
        drop_type_color_rule(&mut config, "SECRET");
        assert_eq!(config.conditional_rules.len(), 1);
        let ConditionalKind::Rule { style, .. } = &config.conditional_rules[0].kind else {
            panic!("plain rule");
        };
        assert!(style.bold, "the user's own rule survives");
    }

    #[test]
    fn convert_cell_bool() {
        assert_eq!(convert_cell(&CoreCell::Bool(true)), GridCell::Boolean(true));
        assert_eq!(
            convert_cell(&CoreCell::Bool(false)),
            GridCell::Boolean(false)
        );
    }

    #[test]
    fn convert_cell_integer() {
        assert_eq!(convert_cell(&CoreCell::I64(42)), GridCell::Integer(42));
        assert_eq!(convert_cell(&CoreCell::I64(-7)), GridCell::Integer(-7));
    }

    #[test]
    fn convert_cell_decimal() {
        assert_eq!(convert_cell(&CoreCell::F64(1.5)), GridCell::Decimal(1.5));
    }

    #[test]
    fn convert_cell_string() {
        assert_eq!(
            convert_cell(&CoreCell::String("hello".into())),
            GridCell::Text("hello".into())
        );
    }

    #[test]
    fn convert_cell_bytes() {
        let result = convert_cell(&CoreCell::Bytes(vec![1, 2, 3]));
        assert!(matches!(result, GridCell::Text(ref s) if s == "<3 bytes>"));
    }

    #[test]
    fn decimal_column_string_cells_coerce_to_numeric() {
        // TDS ships decimal/numeric/money cells as canonical strings; the
        // grid must get numeric cells back or pivot Sum/Avg see only Text
        // and produce blanks.
        assert_eq!(
            coerce_numeric_text(GridCell::Text("-3790.62".into()), ColumnKind::Decimal),
            GridCell::Decimal(-3790.62)
        );
        assert_eq!(
            coerce_numeric_text(GridCell::Text("42".into()), ColumnKind::Decimal),
            GridCell::Integer(42)
        );
        // Unparseable stays text.
        assert_eq!(
            coerce_numeric_text(GridCell::Text("n/a".into()), ColumnKind::Decimal),
            GridCell::Text("n/a".into())
        );
    }

    #[test]
    fn wide_decimal_text_keeps_every_digit() {
        // Wider than f64 or past i64: the grid gets an exact decimal whose
        // digits survive the round trip back to a core cell.
        for wire in [
            "12345678901234567890123",
            "-1234567890.12345678901234567890",
        ] {
            let cell = coerce_numeric_text(GridCell::Text(wire.into()), ColumnKind::Decimal);
            assert!(
                matches!(cell, GridCell::ExactDecimal(_)),
                "{wire}: {cell:?}"
            );
            assert_eq!(grid_cell_to_core(&cell), CoreCell::String(wire.into()));
        }
        // Exponent forms are not plain decimals; they still read as f64.
        assert_eq!(
            coerce_numeric_text(GridCell::Text("1.5E+3".into()), ColumnKind::Decimal),
            GridCell::Decimal(1500.0)
        );
    }

    #[test]
    fn money_column_string_cells_coerce_to_numeric() {
        assert_eq!(
            coerce_numeric_text(GridCell::Text("0.0500".into()), ColumnKind::Decimal),
            GridCell::Decimal(0.05)
        );
    }

    #[test]
    fn text_column_numeric_strings_stay_text() {
        assert_eq!(
            coerce_numeric_text(GridCell::Text("3790.62".into()), ColumnKind::Text),
            GridCell::Text("3790.62".into())
        );
    }

    /// Header tooltips carry the resolved provenance: the reported query's
    /// aliased columns say which base column and table produced them, and
    /// columns without a certain source get no tooltip.
    #[test]
    fn provenance_tooltips_name_the_source_column_and_table() {
        let sql = "SELECT p.Name AS PermissionName, COUNT(*) AS Total FROM Shared.Permission p GROUP BY p.Name";
        let columns = vec![
            ResultColumn {
                name: "PermissionName".into(),
                data_type: "nvarchar".into(),
                nullable: false,
            },
            ResultColumn {
                name: "Total".into(),
                data_type: "int".into(),
                nullable: false,
            },
        ];
        let mut config = GridConfig::default();
        apply_provenance_tooltips(&mut config, &columns, Some(sql));
        assert_eq!(
            config.column_overrides[0].header_tooltip.as_deref(),
            Some("Source: p.Name from Shared.Permission p")
        );
        assert_eq!(config.column_overrides[1].header_tooltip, None);

        // A CTE that hides its body still yields the outer-scope reference.
        let sql = "WITH x AS (SELECT a FROM T UNION ALL SELECT a FROM U) SELECT x.a AS A FROM x";
        let columns = vec![ResultColumn {
            name: "A".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let mut config = GridConfig::default();
        apply_provenance_tooltips(&mut config, &columns, Some(sql));
        assert_eq!(
            config.column_overrides[0].header_tooltip.as_deref(),
            Some("Source: x.a")
        );

        // Multi-statement scripts attribute nothing.
        let mut config = GridConfig::default();
        apply_provenance_tooltips(&mut config, &columns, Some("SELECT 1; SELECT 2"));
        assert!(config.column_overrides.is_empty());
    }

    #[test]
    fn convert_row_with_formats_coerces_decimal_strings() {
        let columns = vec![
            ResultColumn {
                name: "Name".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "Amount".into(),
                data_type: "numeric".into(),
                nullable: true,
            },
        ];
        let row = vec![
            CoreCell::String("Widgets".into()),
            CoreCell::String("1234.5000".into()),
        ];
        let cells = convert_row_with_formats(None, &row, &columns, &[], None);
        assert_eq!(cells[0], GridCell::Text("Widgets".into()));
        assert_eq!(cells[1], GridCell::Decimal(1234.5));
    }

    #[test]
    fn convert_row_with_formats_coerces_integer_strings_via_plan() {
        let columns = vec![ResultColumn {
            name: "Id".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let row = vec![CoreCell::String("42".into())];
        let cells = convert_row_with_formats(None, &row, &columns, &[], None);
        assert_eq!(cells[0], GridCell::Integer(42));
        let batched =
            convert_rows_with_formats(None, std::slice::from_ref(&row), &columns, &[], None);
        assert_eq!(batched[0][0], GridCell::Integer(42));
        let grid = build_grid_data_with_formats(None, &columns, &[row], &[], None)
            .expect("borrowed grid conversion should succeed");
        assert_eq!(grid.rows.len(), 1);
    }

    #[test]
    fn convert_row_with_formats_passes_text_column_through() {
        let columns = vec![ResultColumn {
            name: "Code".into(),
            data_type: "varchar".into(),
            nullable: true,
        }];
        let row = vec![CoreCell::String("3790.62".into())];
        let cells = convert_row_with_formats(None, &row, &columns, &[], None);
        assert_eq!(cells[0], GridCell::Text("3790.62".into()));
    }

    #[test]
    fn convert_row_with_formats_renders_datetime_in_named_zone() {
        use crate::preferences::DateTimeZoneMode;
        let columns = vec![
            ResultColumn {
                name: "CreatedAt".into(),
                data_type: "timestamptz".into(),
                nullable: true,
            },
            ResultColumn {
                name: "BirthDate".into(),
                data_type: "date".into(),
                nullable: true,
            },
        ];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.date_format.time_zone_mode = DateTimeZoneMode::AlwaysInTimeZone;
        prefs.defaults.date_format.time_zone_identifier = "Asia/Tokyo".into();
        let row = vec![
            CoreCell::String("2026-01-01T12:00:00+00:00".into()),
            CoreCell::String("2026-01-01".into()),
        ];
        let cells = convert_row_with_formats(None, &row, &columns, &[], Some(&prefs));
        assert_eq!(cells[0], GridCell::Text("2026-01-01 21:00:00".into()));
        // Date-only columns never shift.
        assert_eq!(cells[1], GridCell::Text("2026-01-01".into()));
        // Without formatting preferences the engine text passes through.
        let raw = convert_row_with_formats(None, &row, &columns, &[], None);
        assert_eq!(raw[0], GridCell::Text("2026-01-01T12:00:00+00:00".into()));
    }

    #[test]
    fn convert_row_with_formats_applies_binary_hex_hint() {
        use crate::query_format::QueryColumnFormat;
        let columns = vec![ResultColumn {
            name: "Blob".into(),
            data_type: "varbinary".into(),
            nullable: true,
        }];
        let row = vec![CoreCell::Bytes(vec![0xDE, 0xAD])];
        let hints = vec![QueryFormatHint {
            column: "Blob".into(),
            format: QueryColumnFormat::Binary(BinaryFormat::Hex),
            width_dp: None,
            zone: None,
        }];
        let cells = convert_row_with_formats(None, &row, &columns, &hints, None);
        assert_eq!(cells[0], GridCell::Text("DEAD".into()));
    }

    /// Sequel Ace #2148: binary(16) ids display as UUID or ULID text; values
    /// of any other length fall back to hex. Owned and borrowed paths agree.
    #[test]
    fn convert_row_with_formats_applies_uuid_and_ulid_hints() {
        use crate::query_format::QueryColumnFormat;
        let columns = vec![
            ResultColumn {
                name: "Id".into(),
                data_type: "binary(16)".into(),
                nullable: true,
            },
            ResultColumn {
                name: "EventId".into(),
                data_type: "varbinary(16)".into(),
                nullable: true,
            },
        ];
        let id: Vec<u8> = vec![
            0x01, 0x7F, 0x22, 0xE2, 0x79, 0xB0, 0x7C, 0xC3, 0x98, 0xC4, 0xDC, 0x0C, 0x0C, 0x07,
            0x39, 0x8F,
        ];
        let hint = |column: &str, format| QueryFormatHint {
            column: column.into(),
            format: QueryColumnFormat::Binary(format),
            width_dp: None,
            zone: None,
        };
        let hints = vec![
            hint("Id", BinaryFormat::Uuid),
            hint("EventId", BinaryFormat::Ulid),
        ];
        let row = vec![CoreCell::Bytes(id.clone()), CoreCell::Bytes(id.clone())];
        let cells = convert_row_with_formats(None, &row, &columns, &hints, None);
        assert_eq!(
            cells[0],
            GridCell::Text("017f22e2-79b0-7cc3-98c4-dc0c0c07398f".into())
        );
        assert_eq!(
            cells[1],
            GridCell::Text("01FWHE4YDGFK1SHH6W1G60EECF".into())
        );
        let owned =
            convert_row_owned_with_plan(row, &column_cell_plan(None, &columns, &hints, None));
        assert_eq!(owned, cells);

        let short = vec![
            CoreCell::Bytes(vec![0xDE, 0xAD]),
            CoreCell::Bytes(vec![0xAB; 17]),
        ];
        let cells = convert_row_with_formats(None, &short, &columns, &hints, None);
        assert_eq!(cells[0], GridCell::Text("DEAD".into()));
        assert_eq!(cells[1], GridCell::Text("AB".repeat(17)));
        let nulls = vec![CoreCell::Null, CoreCell::Null];
        let cells = convert_row_with_formats(None, &nulls, &columns, &hints, None);
        assert_eq!(cells[0], GridCell::None);
    }

    #[test]
    fn convert_row_pads_short_row() {
        let row = vec![CoreCell::I64(1)];
        let result = convert_row(&row, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], GridCell::Integer(1));
        assert_eq!(result[1], GridCell::None);
        assert_eq!(result[2], GridCell::None);
    }

    #[test]
    fn convert_row_truncates_long_row() {
        let row = vec![CoreCell::I64(1), CoreCell::I64(2), CoreCell::I64(3)];
        let result = convert_row(&row, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], GridCell::Integer(1));
        assert_eq!(result[1], GridCell::Integer(2));
    }

    /// The owned conversion must agree with the borrowed one for every
    /// variant, including the pad/truncate rectangularization.
    #[test]
    fn convert_row_owned_matches_borrowed_conversion() {
        let row = vec![
            CoreCell::Null,
            CoreCell::Bool(true),
            CoreCell::I64(7),
            CoreCell::F64(1.5),
            CoreCell::String("hello".into()),
            CoreCell::Bytes(vec![1, 2, 3]),
        ];
        for col_count in [4usize, 6, 8] {
            assert_eq!(
                convert_row_owned(row.clone(), col_count),
                convert_row(&row, col_count),
                "col_count={col_count}"
            );
        }
    }

    #[test]
    fn convert_cell_owned_moves_string() {
        assert_eq!(
            convert_cell_owned(CoreCell::String("moved".into())),
            GridCell::Text("moved".into())
        );
    }

    /// The grid is built from the engine the result came from, so it reads
    /// a column's type the way the WHERE menu and the row editor do: SQL
    /// Server's `timestamp` is its binary rowversion (plain text to the
    /// grid, no date format), PostgreSQL's is a date-time.
    #[test]
    fn grid_reads_timestamp_on_the_results_engine() {
        use crate::where_actions::{WhereValueKind, classify_column};
        let column = ResultColumn {
            name: "rv".into(),
            data_type: "timestamp".into(),
            nullable: true,
        };
        let prefs = ResultFormattingPreferences::default();

        let sql_server = Some(DatabaseType::SqlServer);
        assert_eq!(convert_column(sql_server, &column).kind, ColumnKind::Text);
        let config = build_grid_config(sql_server, &prefs, std::slice::from_ref(&column));
        assert!(config.column_overrides[0].date.is_none());
        assert_eq!(
            classify_column(DatabaseType::SqlServer, &column),
            WhereValueKind::Bytes
        );
        let data = build_grid_data(sql_server, std::slice::from_ref(&column), &[]).unwrap();
        assert_eq!(data.columns[0].kind, ColumnKind::Text);

        let postgres = Some(DatabaseType::Postgres);
        assert_eq!(convert_column(postgres, &column).kind, ColumnKind::Date);
        let config = build_grid_config(postgres, &prefs, std::slice::from_ref(&column));
        assert!(config.column_overrides[0].date.is_some());
        assert_eq!(
            classify_column(DatabaseType::Postgres, &column),
            WhereValueKind::Date
        );
    }

    #[test]
    fn convert_column_int() {
        let col = ResultColumn {
            name: "count".into(),
            data_type: "int".into(),
            nullable: true,
        };
        let grid_col = convert_column(None, &col);
        assert_eq!(grid_col.name, "count");
        assert_eq!(grid_col.kind, ColumnKind::Integer);
        assert_eq!(grid_col.width, MIN_COLUMN_WIDTH);
    }

    /// A short name does not shrink the column below the floor; a long one
    /// widens it far enough to hold the whole header.
    #[test]
    fn auto_width_fits_name_and_values_up_to_the_cap() {
        let advance = Theme::font_size() * CHAR_ADVANCE_RATIO;
        let width = |chars: usize| chars as f32 * advance + HEADER_CHROME_WIDTH;
        // Short values but a longer name: wide enough for the whole name.
        assert_eq!(auto_column_width("CustomerName", 6, 30), width(12));
        // Values wider than the name win.
        assert_eq!(auto_column_width("Id", 20, 30), width(20));
        // Capped at 30 characters; the rest is drawn with an ellipsis.
        assert_eq!(auto_column_width("Id", 200, 30), width(30));
        assert_eq!(auto_column_width(&"N".repeat(45), 4, 30), width(30));
        // 0 means always size to the data.
        assert_eq!(auto_column_width("Id", 200, 0), width(200));
        // Sizes down too, to a small floor.
        assert_eq!(auto_column_width("a", 1, 30), width(AUTO_SIZE_MIN_CHARS));
        assert_eq!(widest_value_cells(["ab", "abcd\nx", "abc"], 4), 4);
        assert_eq!(widest_value_cells(std::iter::empty::<&str>(), 4), 0);
    }

    /// CJK, full-width and emoji text is about two cells per character; a
    /// column sized by character count showed half of it. Tabs count as the
    /// editor's tab width, combining marks as nothing.
    #[test]
    fn value_cells_follow_display_width_not_character_count() {
        let cjk = "東京都渋谷区";
        assert_eq!(cjk.chars().count(), 6);
        assert_eq!(widest_value_cells([cjk], 4), 12);
        let full_width = "ＡＢＣ";
        assert_eq!(widest_value_cells([full_width], 4), 6);
        let emoji = "🙂🙂🙂";
        assert_eq!(widest_value_cells([emoji], 4), 6);
        // Combining accents add no cells.
        assert_eq!(widest_value_cells(["e\u{301}a"], 4), 2);
        // A tab takes the configured width; the widest line still wins.
        assert_eq!(widest_value_cells(["a\tb", "xyz"], 4), 6);
        assert_eq!(widest_value_cells(["a\tb"], 8), 10);
        // The name is measured the same way, so a CJK header fits too.
        let advance = Theme::font_size() * CHAR_ADVANCE_RATIO;
        let width = |cells: usize| cells as f32 * advance + HEADER_CHROME_WIDTH;
        assert_eq!(auto_column_width("名前", 3, 30), width(4));
        assert_eq!(auto_column_width("id", 12, 30), width(12));
        // The cap stays in cells: six wide characters hit a cap of 10 at 10.
        assert_eq!(auto_column_width("id", 12, 10), width(10));
    }

    #[test]
    fn column_width_tracks_the_name_between_floor_and_ceiling() {
        assert_eq!(column_width_for_name("id"), MIN_COLUMN_WIDTH);

        let long = "TransactionSettlementDateUtc";
        let widened = column_width_for_name(long);
        assert!(
            widened > MIN_COLUMN_WIDTH,
            "a 28-character header needs more than the {MIN_COLUMN_WIDTH}px floor, got {widened}"
        );
        assert!(
            widened >= long.chars().count() as f32 * Theme::font_size() * CHAR_ADVANCE_RATIO,
            "the label itself must fit inside the column"
        );

        assert_eq!(column_width_for_name(&"x".repeat(500)), MAX_COLUMN_WIDTH);
    }

    #[test]
    fn convert_column_uuid_maps_to_text() {
        let col = ResultColumn {
            name: "id".into(),
            data_type: "uniqueidentifier".into(),
            nullable: true,
        };
        let grid_col = convert_column(None, &col);
        assert_eq!(grid_col.kind, ColumnKind::Text);
    }

    #[test]
    fn convert_column_bool() {
        let col = ResultColumn {
            name: "active".into(),
            data_type: "bit".into(),
            nullable: true,
        };
        let grid_col = convert_column(None, &col);
        assert_eq!(grid_col.kind, ColumnKind::Boolean);
    }

    #[test]
    fn convert_column_decimal() {
        let col = ResultColumn {
            name: "amount".into(),
            data_type: "decimal(10,2)".into(),
            nullable: true,
        };
        let grid_col = convert_column(None, &col);
        assert_eq!(grid_col.kind, ColumnKind::Decimal);
    }

    #[test]
    fn convert_column_date() {
        let col = ResultColumn {
            name: "created".into(),
            data_type: "datetime".into(),
            nullable: true,
        };
        let grid_col = convert_column(None, &col);
        assert_eq!(grid_col.kind, ColumnKind::Date);
    }

    #[test]
    fn build_grid_data_rectangular() {
        let columns = vec![ResultColumn {
            name: "a".into(),
            data_type: "int".into(),
            nullable: true,
        }];
        let rows = vec![vec![CoreCell::I64(1)], vec![CoreCell::I64(2)]];
        let data = build_grid_data(None, &columns, &rows).expect("rectangular");
        assert_eq!(data.row_count(), 2);
        assert_eq!(data.column_count(), 1);
    }

    #[test]
    fn build_grid_data_pads_ragged_row() {
        let columns = vec![
            ResultColumn {
                name: "a".into(),
                data_type: "int".into(),
                nullable: true,
            },
            ResultColumn {
                name: "b".into(),
                data_type: "int".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CoreCell::I64(1)]]; // only 1 cell, should be padded
        let data = build_grid_data(None, &columns, &rows).expect("padded to rectangular");
        assert_eq!(data.row_count(), 1);
        assert_eq!(data.column_count(), 2);
        assert_eq!(data.cell(0, 0), Some(&GridCell::Integer(1)));
        assert_eq!(data.cell(0, 1), Some(&GridCell::None));
    }

    #[test]
    fn build_grid_data_empty() {
        let data = build_grid_data(None, &[], &[]).expect("empty is valid");
        assert_eq!(data.row_count(), 0);
        assert_eq!(data.column_count(), 0);
    }

    #[test]
    fn convert_alignment_mappings() {
        assert_eq!(
            convert_alignment(&ResultTextAlignment::Left),
            TextAlignment::Left
        );
        assert_eq!(
            convert_alignment(&ResultTextAlignment::Center),
            TextAlignment::Center
        );
        assert_eq!(
            convert_alignment(&ResultTextAlignment::Right),
            TextAlignment::Right
        );
    }

    #[test]
    fn integer_number_format_basic() {
        let fmt = IntegerFormat::default();
        let nf = integer_number_format(&fmt);
        assert_eq!(nf.decimals, 0);
        assert!(nf.thousands_separator);
        assert_eq!(nf.alignment, TextAlignment::Right);
    }

    #[test]
    fn integer_number_format_parentheses() {
        let fmt = IntegerFormat {
            negative_format: NegativeFormatStyle::Parentheses,
            ..Default::default()
        };
        let nf = integer_number_format(&fmt);
        assert!(nf.negative_parentheses);
    }

    #[test]
    fn decimal_number_format_places() {
        let fmt = DecimalFormat {
            decimal_places: 4,
            ..Default::default()
        };
        let nf = decimal_number_format(&fmt);
        assert_eq!(nf.decimals, 4);
    }

    #[test]
    fn decimal_number_format_negative_places_clamped() {
        let fmt = DecimalFormat {
            decimal_places: -1,
            ..Default::default()
        };
        let nf = decimal_number_format(&fmt);
        assert_eq!(nf.decimals, 0);
    }

    #[test]
    fn boolean_format_yn() {
        let bf = boolean_format(&BoolFormatStyle::YN, "", "", &ResultTextAlignment::Center);
        assert_eq!(bf.true_text, "y");
        assert_eq!(bf.false_text, "n");
        assert_eq!(bf.alignment, TextAlignment::Center);
    }

    #[test]
    fn boolean_format_custom() {
        let bf = boolean_format(
            &BoolFormatStyle::Custom,
            "oui",
            "non",
            &ResultTextAlignment::Left,
        );
        assert_eq!(bf.true_text, "oui");
        assert_eq!(bf.false_text, "non");
    }

    #[test]
    fn swift_date_format_translation() {
        use crate::result_cell_format::swift_date_format_to_strftime;
        assert_eq!(
            swift_date_format_to_strftime("yyyy-MM-dd HH:mm:ss"),
            "%Y-%m-%d %H:%M:%S"
        );
        assert_eq!(swift_date_format_to_strftime("yyyy-MM-dd"), "%Y-%m-%d");
    }

    #[test]
    fn build_grid_config_default() {
        let prefs = ResultFormattingPreferences::default();
        let columns = vec![ResultColumn {
            name: "count".into(),
            data_type: "int".into(),
            nullable: true,
        }];
        let config = build_grid_config(None, &prefs, &columns);
        assert_eq!(config.column_overrides.len(), 1);
        // Integer column should have a number override with 0 decimals.
        assert!(config.column_overrides[0].number.is_some());
        let num = config.column_overrides[0].number.as_ref().unwrap();
        assert_eq!(num.decimals, 0);
        assert!(config.default_null.italic);
        assert!(!config.default_null.background);
        assert_eq!(config.default_null.text, "NULL");
    }

    #[test]
    fn build_grid_config_with_column_override() {
        let mut prefs = ResultFormattingPreferences::default();
        let mut override_fmt =
            crate::preferences::ColumnFormat::default_for_kind(ResultKind::Decimal);
        override_fmt.decimal_format.decimal_places = 4;
        prefs
            .column_overrides
            .insert("amount".to_string(), override_fmt);

        let columns = vec![ResultColumn {
            name: "amount".into(),
            data_type: "decimal(10,2)".into(),
            nullable: true,
        }];
        let config = build_grid_config(None, &prefs, &columns);
        let num = config.column_overrides[0]
            .number
            .as_ref()
            .expect("decimal column has number override");
        assert_eq!(num.decimals, 4);
    }

    #[test]
    fn build_grid_config_text_column_no_number_override() {
        let prefs = ResultFormattingPreferences::default();
        let columns = vec![ResultColumn {
            name: "name".into(),
            data_type: "varchar".into(),
            nullable: true,
        }];
        let config = build_grid_config(None, &prefs, &columns);
        assert!(config.column_overrides[0].number.is_none());
    }

    #[test]
    fn build_grid_theme_matches_theme_accessors() {
        let _guard = crate::theme::THEME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for kind in [
            crate::theme::ThemeKind::Dark,
            crate::theme::ThemeKind::Light,
            crate::theme::ThemeKind::HighContrast,
        ] {
            crate::theme::set_kind(kind);
            let theme = build_grid_theme();
            assert_eq!(theme.bg, Theme::bg());
            assert_eq!(theme.header_bg, Theme::panel_bg());
            assert_eq!(theme.header_fg, Theme::text());
            assert_eq!(theme.text_fg, Theme::text());
            assert_eq!(theme.muted_text, Theme::text_dim());
            assert_eq!(theme.null_fg, Theme::text_dim());
            assert_eq!(theme.selection_bg, Theme::selection_fill());
            assert_eq!(theme.alt_row_bg, Theme::row_alt());
            assert_eq!(theme.grid_line, Theme::border());
            assert_eq!(theme.sort_indicator, Theme::accent());
            assert_eq!(theme.menu_bg, Theme::popup_bg());
            assert_eq!(theme.menu_fg, Theme::text());
            assert_eq!(theme.menu_hover_bg, Theme::completion_selected());
        }
        crate::theme::set_kind(crate::theme::ThemeKind::System);
    }

    /// Every `SqllyDataTable::builder(...)` call in this crate must pass the
    /// theme through `.theme(...)`. A theme written to `state.theme` after
    /// `build` leaves the widget in follow-OS mode, where each render
    /// re-resolves the crate's own default palette and overwrites the app
    /// theme — the grid then ignores the theme picked in Preferences. This
    /// scan trips if a new construction site regresses to the post-build
    /// pattern.
    #[test]
    fn every_grid_builder_passes_the_theme() {
        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    scan(&path, offenders);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read source file");
                let mut search_from = 0;
                // Needle split so this test's own source doesn't match it.
                let needle = concat!("SqllyDataTable::", "builder(");
                while let Some(found) = source[search_from..].find(needle) {
                    let at = search_from + found;
                    // The builder chain ends at `.build(cx)`; `.theme(` must
                    // appear within it.
                    let chain_end = source[at..]
                        .find(".build(")
                        .map(|e| at + e)
                        .unwrap_or(source.len());
                    if !source[at..chain_end].contains(".theme(") {
                        let line = source[..at].lines().count();
                        offenders.push(format!("{}:{line}", path.display()));
                    }
                    search_from = at + 1;
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        scan(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "grid built without a builder theme (host theme would be \
             overwritten on render): {offenders:?}"
        );
    }

    #[test]
    fn build_grid_theme_uses_app_colors() {
        let _guard = crate::theme::THEME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::theme::set_kind(crate::theme::ThemeKind::Dark);
        let theme = build_grid_theme();
        assert_eq!(theme.bg, Theme::bg());
        assert_eq!(theme.text_fg, Theme::text());
        assert_eq!(theme.grid_line, Theme::border());
        assert_eq!(theme.menu_bg, Theme::popup_bg());
        assert_eq!(theme.menu_hover_bg, Theme::completion_selected());
        assert_eq!(theme.menu_fg, Theme::text());
        crate::theme::set_kind(crate::theme::ThemeKind::System);
    }

    #[test]
    fn grid_data_applies_json_path_override() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.column_overrides.insert(
            "payload".into(),
            crate::preferences::ColumnFormat {
                kind: ResultKind::JsonPath,
                json_path_format: crate::preferences::JsonPathFormat {
                    path: "$.a".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let columns = vec![ResultColumn {
            name: "payload".into(),
            data_type: "nvarchar".into(),
            nullable: true,
        }];
        let rows = vec![vec![CoreCell::String(r#"{"a":1}"#.into())]];
        let data = build_grid_data_with_formats(None, &columns, &rows, &[], Some(&prefs))
            .expect("grid data");
        assert_eq!(data.rows[0][0], GridCell::Text("1".into()));
    }
}

/// Settings › Results formats (Unique ID, Integer, Decimal), `-- sqlly
/// format:` directives, and qualified Column overrides must reach the cells
/// the grid paints — asserted on the exact displayed strings.
#[cfg(test)]
mod settings_reach_grid_tests {
    use super::*;
    use crate::preferences::{DecimalSeparator, ThousandsSeparator};
    use crate::query_format::{QueryColumnFormat, QueryFormatHint};
    use sqlly_datatable::format::format_cell;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    /// What the grid paints for each cell of `row`, plus each column's
    /// alignment: the same conversion + config + formatter the result grid
    /// runs.
    fn displayed(
        prefs: &ResultFormattingPreferences,
        columns: &[ResultColumn],
        hints: &[QueryFormatHint],
        row: Vec<CoreCell>,
    ) -> Vec<(String, TextAlignment)> {
        let mut config = build_grid_config(None, prefs, columns);
        crate::query_format::apply_to_grid_config(&mut config, columns, hints);
        let grid_columns: Vec<Column> = columns.iter().map(|c| convert_column(None, c)).collect();
        let resolved = config.resolve_all(&grid_columns);
        let cells = convert_row_with_formats(None, &row, columns, hints, Some(prefs));
        cells
            .iter()
            .zip(&resolved)
            .map(|(cell, fmt)| (format_cell(cell, fmt).0, fmt.alignment()))
            .collect()
    }

    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn uuid_show_dashes_off_strips_dashes_in_the_grid_only() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.uuid_format.show_dashes = false;
        prefs.defaults.uuid_format.alignment = ResultTextAlignment::Right;
        let columns = vec![col("Id", "uniqueidentifier")];
        let shown = displayed(&prefs, &columns, &[], vec![CoreCell::String(UUID.into())]);
        assert_eq!(
            shown,
            vec![(
                "550e8400e29b41d4a716446655440000".to_string(),
                TextAlignment::Right
            )]
        );
        // The cell value — what copy, export, and editing read — keeps the
        // UUID exactly as the engine returned it.
        let cells = convert_row_with_formats(
            None,
            &[CoreCell::String(UUID.into())],
            &columns,
            &[],
            Some(&prefs),
        );
        assert_eq!(cells[0], GridCell::Text(UUID.into()));
    }

    #[test]
    fn uuid_defaults_keep_dashes_and_follow_alignment() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.uuid_format.alignment = ResultTextAlignment::Center;
        let shown = displayed(
            &prefs,
            &[col("Id", "uuid")],
            &[],
            vec![CoreCell::String(UUID.into())],
        );
        assert_eq!(shown, vec![(UUID.to_string(), TextAlignment::Center)]);
        // The Settings sample says the same thing.
        assert_eq!(prefs.format_uuid(UUID), UUID);
    }

    #[test]
    fn uuid_column_override_settings_win() {
        let mut prefs = ResultFormattingPreferences::default();
        let mut fmt = ColumnFormat::default_for_kind(ResultKind::Uuid);
        fmt.uuid_format.show_dashes = false;
        prefs.column_overrides.insert("Ref".into(), fmt);
        let shown = displayed(
            &prefs,
            &[col("Ref", "nvarchar(36)")],
            &[],
            vec![CoreCell::String(UUID.into())],
        );
        assert_eq!(shown[0].0, "550e8400e29b41d4a716446655440000");
    }

    #[test]
    fn decimal_separator_and_thousands_character_reach_decimal_cells() {
        let cases = [
            (
                DecimalSeparator::Comma,
                ThousandsSeparator::Period,
                "1.234.567,89",
            ),
            (
                DecimalSeparator::Comma,
                ThousandsSeparator::Space,
                "1 234 567,89",
            ),
            (
                DecimalSeparator::Period,
                ThousandsSeparator::Apostrophe,
                "1'234'567.89",
            ),
            (
                DecimalSeparator::Comma,
                ThousandsSeparator::None,
                "1234567,89",
            ),
            (
                DecimalSeparator::Period,
                ThousandsSeparator::Comma,
                "1,234,567.89",
            ),
        ];
        for (decimal, thousands, expected) in cases {
            let mut prefs = ResultFormattingPreferences::default();
            prefs.defaults.decimal_format.decimal_places = 2;
            prefs.defaults.decimal_format.decimal_separator = decimal;
            prefs.defaults.decimal_format.thousands_separator = thousands;
            // Typed doubles and the TDS layer's decimal-as-text both render.
            let shown = displayed(
                &prefs,
                &[col("Amount", "decimal(18,4)"), col("Rate", "float")],
                &[],
                vec![
                    CoreCell::String("1234567.8912".into()),
                    CoreCell::F64(1_234_567.891),
                ],
            );
            assert_eq!(shown[0].0, expected);
            assert_eq!(shown[1].0, expected);
            // The Settings sample uses the same formatter.
            assert_eq!(prefs.format_decimal(1_234_567.891), expected);
        }
    }

    #[test]
    fn decimal_grouping_off_ignores_the_thousands_character() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.decimal_format.decimal_places = 2;
        prefs.defaults.decimal_format.use_thousands_separators = false;
        prefs.defaults.decimal_format.decimal_separator = DecimalSeparator::Comma;
        prefs.defaults.decimal_format.thousands_separator = ThousandsSeparator::Period;
        let shown = displayed(
            &prefs,
            &[col("Amount", "numeric")],
            &[],
            vec![CoreCell::F64(-1_234_567.891)],
        );
        assert_eq!(shown[0].0, "-1234567,89");
    }

    #[test]
    fn decimal_reduce_large_numbers_reaches_decimal_cells() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.decimal_format.decimal_places = 2;
        prefs.defaults.decimal_format.reduce_large_to_metric = true;
        prefs.defaults.decimal_format.decimal_separator = DecimalSeparator::Comma;
        prefs.defaults.decimal_format.thousands_separator = ThousandsSeparator::Period;
        let columns = vec![col("Amount", "decimal(18,2)")];
        let show = |v: f64| {
            displayed(&prefs, &columns, &[], vec![CoreCell::F64(v)])[0]
                .0
                .clone()
        };
        assert_eq!(show(999.5), "999,50");
        assert_eq!(show(2_340_000.0), "2,34M");
        assert_eq!(show(-1_500.25), "-1,50K");
        assert_eq!(prefs.format_decimal(2_340_000.0), "2,34M");
        // The cell stays numeric, so sorting and numeric filters see the
        // exact value.
        let cells = convert_row_with_formats(
            None,
            &[CoreCell::String("2340000.00".into())],
            &columns,
            &[],
            Some(&prefs),
        );
        assert_eq!(cells[0], GridCell::Decimal(2_340_000.0));
    }

    #[test]
    fn integer_reduce_large_numbers_reaches_integer_cells_like_the_sample() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.integer_format.reduce_large_to_metric = true;
        let columns = vec![col("Visits", "bigint")];
        for (value, expected) in [
            (999, "999"),
            (1_500, "1.50K"),
            (1_234_567, "1.23M"),
            (-2_500_000_000, "-2.50G"),
        ] {
            let shown = displayed(&prefs, &columns, &[], vec![CoreCell::I64(value)]);
            assert_eq!(shown[0].0, expected, "{value}");
            assert_eq!(prefs.format_integer(value), expected, "sample for {value}");
        }
        // The configured threshold decides what is reduced.
        prefs.defaults.integer_format.metric_threshold = 10_000;
        let shown = displayed(&prefs, &columns, &[], vec![CoreCell::I64(5_000)]);
        assert_eq!(shown[0].0, "5,000");
        assert_eq!(prefs.format_integer(5_000), "5,000");
        let shown = displayed(&prefs, &columns, &[], vec![CoreCell::I64(15_000)]);
        assert_eq!(shown[0].0, "15.00K");
        assert_eq!(prefs.format_integer(15_000), "15.00K");
        // Whole units round at the step-up ratio when decimals are 0.
        prefs.defaults.integer_format.metric_decimals = 0;
        let shown = displayed(&prefs, &columns, &[], vec![CoreCell::I64(15_600)]);
        assert_eq!(shown[0].0, "16K");
        assert_eq!(prefs.format_integer(15_600), "16K");
    }

    #[test]
    fn integers_group_with_a_comma_whatever_the_decimal_characters() {
        // The integer settings have no grouping character of their own; the
        // Settings sample groups with a comma, and so does the grid.
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.decimal_format.thousands_separator = ThousandsSeparator::Period;
        prefs.defaults.decimal_format.decimal_separator = DecimalSeparator::Comma;
        let shown = displayed(
            &prefs,
            &[col("Id", "int")],
            &[],
            vec![CoreCell::I64(1_234_567)],
        );
        assert_eq!(shown[0].0, "1,234,567");
        assert_eq!(prefs.format_integer(1_234_567), "1,234,567");
    }

    fn date_hint(column: &str, pattern: &str) -> QueryFormatHint {
        QueryFormatHint {
            column: column.into(),
            format: QueryColumnFormat::Date {
                format: pattern.into(),
            },
            width_dp: None,
            zone: None,
        }
    }

    #[test]
    fn date_directive_formats_datetime_and_date_columns() {
        let prefs = ResultFormattingPreferences::default();
        let columns = vec![
            col("CreatedAt", "datetime2"),
            col("BirthDate", "date"),
            col("Note", "nvarchar(50)"),
        ];
        let hints = vec![
            date_hint("createdat", "%d/%m/%Y %H:%M"),
            date_hint("BirthDate", "%d.%m.%Y"),
        ];
        let shown = displayed(
            &prefs,
            &columns,
            &hints,
            vec![
                CoreCell::String("2026-07-14 09:30:15.1234567".into()),
                CoreCell::String("2026-07-14".into()),
                CoreCell::String("2026-07-14".into()),
            ],
        );
        assert_eq!(shown[0].0, "14/07/2026 09:30");
        assert_eq!(shown[1].0, "14.07.2026");
        // Columns without a directive are untouched.
        assert_eq!(shown[2].0, "2026-07-14");
        // The directive keeps the column's configured date alignment.
        assert_eq!(shown[0].1, TextAlignment::Left);
    }

    #[test]
    fn date_directive_menu_choices_render() {
        // The three Date choices "Set Formatting Override…" writes.
        let prefs = ResultFormattingPreferences::default();
        let columns = vec![col("At", "datetimeoffset")];
        let raw = || vec![CoreCell::String("2026-07-14T09:30:15+02:00".into())];
        let show = |pattern: &str| {
            displayed(&prefs, &columns, &[date_hint("At", pattern)], raw())[0]
                .0
                .clone()
        };
        // Default display zone is UTC.
        assert_eq!(show("%Y-%m-%d"), "2026-07-14");
        assert_eq!(show("%Y-%m-%d %H:%M:%S"), "2026-07-14 07:30:15");
        assert_eq!(show("%Y-%m-%d %H:%M:%S %:z"), "2026-07-14 07:30:15 +00:00");
    }

    #[test]
    fn date_directive_respects_display_zone_and_as_stored() {
        use crate::preferences::DateTimeZoneMode;
        let columns = vec![col("At", "timestamptz")];
        let hints = vec![date_hint("At", "%Y-%m-%d %H:%M %:z")];
        let row = || vec![CoreCell::String("2026-01-01 12:00:00+02".into())];
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.date_format.time_zone_mode = DateTimeZoneMode::AlwaysInTimeZone;
        prefs.defaults.date_format.time_zone_identifier = "Asia/Tokyo".into();
        assert_eq!(
            displayed(&prefs, &columns, &hints, row())[0].0,
            "2026-01-01 19:00 +09:00"
        );
        prefs.defaults.date_format.time_zone_mode = DateTimeZoneMode::NativeAsStored;
        assert_eq!(
            displayed(&prefs, &columns, &hints, row())[0].0,
            "2026-01-01 12:00 +02:00"
        );
    }

    #[test]
    fn date_directive_never_breaks_on_bad_patterns_or_values() {
        let prefs = ResultFormattingPreferences::default();
        let columns = vec![col("At", "datetime")];
        // An unknown specifier keeps the engine text instead of panicking.
        let shown = displayed(
            &prefs,
            &columns,
            &[date_hint("At", "%Y-%Q")],
            vec![CoreCell::String("2026-07-14 09:30:15".into())],
        );
        assert_eq!(shown[0].0, "2026-07-14 09:30:15");
        // Unparseable text and NULLs pass through.
        let shown = displayed(
            &prefs,
            &columns,
            &[date_hint("At", "%d/%m/%Y")],
            vec![CoreCell::String("not a date".into())],
        );
        assert_eq!(shown[0].0, "not a date");
        let cells = convert_row_with_formats(
            None,
            &[CoreCell::Null],
            &columns,
            &[date_hint("At", "%d/%m/%Y")],
            Some(&prefs),
        );
        assert_eq!(cells[0], GridCell::None);
    }

    #[test]
    fn number_directive_keeps_the_configured_characters() {
        let mut prefs = ResultFormattingPreferences::default();
        prefs.defaults.decimal_format.decimal_separator = DecimalSeparator::Comma;
        prefs.defaults.decimal_format.thousands_separator = ThousandsSeparator::Period;
        prefs.defaults.decimal_format.reduce_large_to_metric = true;
        let hints = vec![QueryFormatHint {
            column: "Amount".into(),
            format: QueryColumnFormat::Number {
                decimals: 1,
                thousands: true,
            },
            width_dp: None,
            zone: None,
        }];
        let shown = displayed(
            &prefs,
            &[col("Amount", "money")],
            &hints,
            vec![CoreCell::F64(1_234_567.26)],
        );
        // Fixed places from the directive: no metric reduction.
        assert_eq!(shown[0].0, "1.234.567,3");
    }

    #[test]
    fn qualified_column_override_reaches_the_grid() {
        let sql = "SELECT o.Total, c.Total AS CustomerTotal FROM Sales.Orders o JOIN Sales.Customers c ON c.Id = o.CustomerId";
        let columns = vec![col("Total", "int"), col("CustomerTotal", "int")];
        let mut prefs = ResultFormattingPreferences::default();
        let mut orders_total = ColumnFormat::default_for_kind(ResultKind::Integer);
        orders_total.integer_format.use_thousands_separators = false;
        prefs
            .column_overrides
            .insert("orders.total".into(), orders_total);
        let mut customers_total = ColumnFormat::default_for_kind(ResultKind::Integer);
        customers_total.integer_format.negative_format = NegativeFormatStyle::Parentheses;
        prefs
            .column_overrides
            .insert("Sales.Customers.Total".into(), customers_total);
        let sources = column_sources(&columns, Some(sql));
        crate::result_cell_format::resolve_qualified_overrides(&mut prefs, &columns, &sources);
        let shown = displayed(
            &prefs,
            &columns,
            &[],
            vec![CoreCell::I64(1_234_567), CoreCell::I64(-1_234)],
        );
        assert_eq!(shown[0].0, "1234567");
        assert_eq!(shown[1].0, "(1,234)");
    }

    #[test]
    fn column_sources_use_provenance_then_the_single_table() {
        let columns = vec![col("PermissionName", "nvarchar"), col("Total", "int")];
        let sources = column_sources(
            &columns,
            Some(
                "SELECT p.Name AS PermissionName, COUNT(*) AS Total FROM Shared.Permission p GROUP BY p.Name",
            ),
        );
        assert_eq!(
            sources[0],
            Some(crate::result_cell_format::ColumnSource {
                table: vec!["Shared".into(), "Permission".into()],
                column: "Name".into(),
            })
        );
        assert_eq!(sources[1], None);
        assert_eq!(column_sources(&columns, None), vec![None, None]);
    }
}
