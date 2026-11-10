//! `-- sqlly column: <Name> = <formula>` — computed columns written as Excel
//! formulas over the result's own columns (see [`crate::formula`]).
//!
//! ```text
//! -- sqlly column: Margin % = IF(!Revenue = 0, 0, (!Revenue - !Cost) / !Revenue); after=Cost
//! -- sqlly column: Running Total = !Amount + N(!"Running Total"[1])
//! ```
//!
//! A computed column joins every result set that has the columns it reads
//! (or only result `n` with `result=n`), at the end or right after the
//! column named by `after=`. Values are computed as rows arrive, in the
//! order the query returned them, so `[1]` always means the row the
//! database sent before this one. Computed columns are evaluated in the
//! order they are declared: a formula may read an earlier computed column
//! on the same row, and any computed column (itself included) on earlier
//! rows.

use sqlly_core::{CellValue, ResultColumn};

use crate::formula::{self, EvalEnv, Expr, Formula, Value};

/// One `-- sqlly column:` line.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedColumn {
    pub name: String,
    pub formula: Formula,
    /// Place the column right after this one (else at the end).
    pub after: Option<String>,
    /// Only this 1-based result set (else every result that can compute it).
    pub result: Option<usize>,
}

/// Every computed column in `sql` (lock blocks included), in declaration
/// order; a later line for the same name replaces the earlier one in place.
pub fn parse(sql: &str) -> Vec<ComputedColumn> {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let mut out: Vec<ComputedColumn> = Vec::new();
    for comment in crate::result_layout::standalone_comments(&sql) {
        let Some(crate::sqlly_directive::Directive::Column { body }) =
            crate::sqlly_directive::classify_body(&comment.body)
        else {
            continue;
        };
        let Ok(column) = parse_body(body) else {
            continue;
        };
        match out
            .iter_mut()
            .find(|c| c.name.eq_ignore_ascii_case(&column.name))
        {
            Some(existing) => *existing = column,
            None => out.push(column),
        }
    }
    out
}

/// Split `text` at top-level `;` — outside strings, quoted names, and
/// parentheses — so a formula keeps any `;` inside its own text.
fn split_options(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            '(' if !in_quote => depth += 1,
            ')' if !in_quote => depth -= 1,
            ';' if !in_quote && depth <= 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Parse a `column:` body: `<Name> = <formula>[; after=<col>][; result=<n>]`.
pub fn parse_body(body: &str) -> Result<ComputedColumn, String> {
    let (name, rest) = body.split_once('=').ok_or_else(|| {
        crate::localization::text("results.computed_columns.expected_name_formula")
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::localization::text(
            "results.computed_columns.name_required",
        ));
    }
    let mut parts = split_options(rest).into_iter();
    let source = parts.next().unwrap_or("").trim();
    let formula = formula::parse(source).map_err(|e| e.message)?;
    let mut after = None;
    let mut result = None;
    for option in parts {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }
        let (key, value) = option.split_once('=').ok_or_else(|| {
            crate::localization::format(
                "results.computed_columns.invalid_option",
                &[("option", option)],
            )
        })?;
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "after" if !value.is_empty() => after = Some(value.trim_matches('"').to_string()),
            "result" => {
                result = Some(value.parse::<usize>().ok().filter(|n| *n >= 1).ok_or_else(
                    || {
                        crate::localization::format(
                            "results.computed_columns.invalid_result_number",
                            &[("value", value)],
                        )
                    },
                )?);
            }
            other => {
                return Err(crate::localization::format(
                    "results.computed_columns.unknown_option",
                    &[("option_name", other)],
                ));
            }
        }
    }
    Ok(ComputedColumn {
        name: name.to_string(),
        formula,
        after,
        result,
    })
}

/// The `-- sqlly column:` line for a column.
pub fn directive_line(name: &str, formula: &str, after: Option<&str>) -> String {
    let formula = formula.trim();
    let formula = formula.strip_prefix('=').unwrap_or(formula).trim();
    let mut line = format!("-- sqlly column: {} = {formula}", name.trim());
    if let Some(after) = after.filter(|a| !a.trim().is_empty()) {
        line.push_str(&format!("; after={}", after.trim()));
    }
    line
}

/// Write `line` into `buffer`: over the `-- sqlly column:` line for
/// `replacing` (or for the line's own name) when there is one — anywhere in
/// the buffer, lock block included — else as a new line at the top of
/// `region`. Returns the text and the region carried across the edit.
pub fn upsert_line(
    buffer: &str,
    region: (usize, usize),
    line: &str,
    replacing: Option<&str>,
) -> (String, (usize, usize)) {
    let new_name = parse_body(line.split_once("column:").map(|(_, b)| b).unwrap_or(""))
        .map(|c| c.name)
        .ok();
    let targets: Vec<&str> = replacing.into_iter().chain(new_name.as_deref()).collect();
    let existing = crate::result_layout::standalone_comments(buffer)
        .into_iter()
        .find(|c| {
            matches!(
                crate::sqlly_directive::classify_body(&c.body),
                Some(crate::sqlly_directive::Directive::Column { body })
                    if parse_body(body).is_ok_and(|spec| targets.iter().any(|t| t.eq_ignore_ascii_case(&spec.name)))
            )
        });
    let (start, end, replacement) = match existing {
        Some(c) => (c.start, c.end, line.to_string()),
        None => {
            let at = crate::result_header_actions::directive_insert_offset(
                buffer,
                region.0.min(buffer.len()),
            );
            let text = if at == buffer.len() && !buffer.is_empty() && !buffer.ends_with('\n') {
                format!("\n{line}")
            } else {
                format!("{line}\n")
            };
            (at, at, text)
        }
    };
    let mut text = buffer.to_string();
    text.replace_range(start..end, &replacement);
    let delta = replacement.len() as isize - (end - start) as isize;
    let shift = |p: usize| {
        if p >= end {
            (p as isize + delta).max(0) as usize
        } else {
            p
        }
    };
    let span = (
        region.0.min(start),
        shift(region.1).max(start + replacement.len()),
    );
    (text, span)
}

// ---------------------------------------------------------------------------
// Applying to a result set
// ---------------------------------------------------------------------------

/// What a result column holds, for reading its cells as formula values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Number,
    Date,
    Other,
}

fn source_kind(data_type: &str) -> SourceKind {
    let t = data_type.to_ascii_lowercase();
    if t.contains("date") || t.contains("time") {
        SourceKind::Date
    } else if [
        "int", "dec", "num", "money", "float", "real", "double", "serial",
    ]
    .iter()
    .any(|k| t.contains(k))
    {
        SourceKind::Number
    } else {
        SourceKind::Other
    }
}

/// What a computed column displays as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    Number,
    /// Whole numbers (DAYS, YEAR, LEN, COUNT…), shown without decimals.
    Integer,
    Date,
    DateTime,
    Text,
    Bool,
}

impl ResultKind {
    fn data_type(self) -> &'static str {
        match self {
            ResultKind::Number => "float",
            ResultKind::Integer => "int",
            ResultKind::Date => "date",
            ResultKind::DateTime => "datetime2",
            ResultKind::Text => "nvarchar",
            ResultKind::Bool => "bit",
        }
    }
}

/// Where each output column comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Source(usize),
    Computed(usize),
}

/// The computed columns of one result set: the output column order and the
/// formulas, ready to extend each arriving row.
#[derive(Debug, Clone)]
pub struct ComputedPlan {
    pub layout: Vec<Slot>,
    pub columns: Vec<ComputedColumn>,
    kinds: Vec<ResultKind>,
    /// Output column names, for reference lookup.
    names: Vec<String>,
    source_kinds: Vec<SourceKind>,
    source_width: usize,
}

impl ComputedPlan {
    /// Plan `specs` against a result set's `columns` (`slot` 0-based).
    /// `None` when no computed column applies.
    pub fn new(specs: &[ComputedColumn], columns: &[ResultColumn], slot: usize) -> Option<Self> {
        let has = |name: &str, extra: &[ComputedColumn]| {
            columns.iter().any(|c| c.name.eq_ignore_ascii_case(name))
                || extra.iter().any(|c| c.name.eq_ignore_ascii_case(name))
        };
        let mut chosen: Vec<ComputedColumn> = Vec::new();
        for spec in specs {
            if spec.result.is_some_and(|r| r != slot + 1) {
                continue;
            }
            // A column the result already has is not shadowed.
            if columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&spec.name))
            {
                continue;
            }
            let mut with_self = chosen.clone();
            with_self.push(spec.clone());
            if spec
                .formula
                .referenced_columns()
                .iter()
                .all(|r| has(r, &with_self))
            {
                chosen.push(spec.clone());
            }
        }
        if chosen.is_empty() {
            return None;
        }
        let mut layout: Vec<Slot> = (0..columns.len()).map(Slot::Source).collect();
        let mut names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        for (k, spec) in chosen.iter().enumerate() {
            let at = spec
                .after
                .as_ref()
                .and_then(|a| names.iter().position(|n| n.eq_ignore_ascii_case(a)))
                .map(|i| i + 1)
                .unwrap_or(names.len());
            layout.insert(at, Slot::Computed(k));
            names.insert(at, spec.name.clone());
        }
        let source_kinds: Vec<SourceKind> =
            columns.iter().map(|c| source_kind(&c.data_type)).collect();
        let mut plan = ComputedPlan {
            layout,
            columns: chosen,
            kinds: Vec::new(),
            names,
            source_kinds,
            source_width: columns.len(),
        };
        // In declaration order: a formula reading an earlier computed column
        // takes that column's kind.
        for k in 0..plan.columns.len() {
            let kind = plan.infer(&plan.columns[k].formula.expr, k);
            plan.kinds.push(kind);
        }
        Some(plan)
    }

    /// The output columns: the source columns with the computed ones placed.
    pub fn output_columns(&self, source: &[ResultColumn]) -> Vec<ResultColumn> {
        self.layout
            .iter()
            .map(|slot| match slot {
                Slot::Source(i) => source[*i].clone(),
                Slot::Computed(k) => ResultColumn {
                    name: self.columns[*k].name.clone(),
                    data_type: self.kinds[*k].data_type().to_string(),
                    nullable: true,
                },
            })
            .collect()
    }

    /// The output position of source column `i`.
    pub fn output_index_of_source(&self, i: usize) -> Option<usize> {
        self.layout.iter().position(|s| *s == Slot::Source(i))
    }

    /// The column names from `a` to `b` in output order.
    fn between(&self, a: &str, b: &str) -> Option<Vec<String>> {
        let i = self.names.iter().position(|n| n.eq_ignore_ascii_case(a))?;
        let j = self.names.iter().position(|n| n.eq_ignore_ascii_case(b))?;
        Some(self.names[i.min(j)..=i.max(j)].to_vec())
    }

    fn infer(&self, expr: &Expr, own: usize) -> ResultKind {
        use ResultKind as K;
        match expr {
            Expr::Number(n) if n.fract() == 0.0 => K::Integer,
            Expr::Number(_) | Expr::Percent(_) => K::Number,
            Expr::Neg(e) => match self.infer(e, own) {
                K::Integer => K::Integer,
                _ => K::Number,
            },
            Expr::Text(_) => K::Text,
            Expr::Bool(_) => K::Bool,
            Expr::Error(_) | Expr::Missing | Expr::Var(_) | Expr::Range(..) => K::Text,
            Expr::Ref(r) => {
                let Some(pos) = self
                    .names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(&r.column))
                else {
                    return K::Text;
                };
                match self.layout[pos] {
                    Slot::Source(i) => match self.source_kinds[i] {
                        SourceKind::Number => K::Number,
                        SourceKind::Date => K::DateTime,
                        SourceKind::Other => K::Text,
                    },
                    Slot::Computed(k) if k < own => self.kinds.get(k).copied().unwrap_or(K::Number),
                    Slot::Computed(_) => K::Number,
                }
            }
            Expr::Binary(op, a, b) => match *op {
                "&" => K::Text,
                "=" | "<>" | "<" | ">" | "<=" | ">=" => K::Bool,
                "+" | "-" => {
                    let (x, y) = (self.infer(a, own), self.infer(b, own));
                    let dated = |k| matches!(k, K::Date | K::DateTime);
                    if dated(x) && dated(y) && *op == "-" {
                        K::Number
                    } else if dated(x) {
                        x
                    } else if dated(y) && *op == "+" {
                        y
                    } else {
                        K::Number
                    }
                }
                _ => K::Number,
            },
            Expr::Call(name, args) => match name.as_str() {
                "DATE" | "TODAY" | "EDATE" | "EOMONTH" | "WORKDAY" | "DATEVALUE" => K::Date,
                "NOW" => K::DateTime,
                "IF" | "IFS" | "IFERROR" | "IFNA" | "SWITCH" | "CHOOSE" | "COALESCE" => {
                    let skip = match name.as_str() {
                        "IF" | "CHOOSE" | "SWITCH" => 1,
                        _ => 0,
                    };
                    let branches: Vec<ResultKind> = args
                        .iter()
                        .skip(skip)
                        .enumerate()
                        .filter(|(i, _)| name != "IFS" || i % 2 == 1)
                        .map(|(_, a)| self.infer(a, own))
                        .collect();
                    combine_kinds(&branches)
                }
                "DAYS" | "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" | "WEEKDAY"
                | "WEEKNUM" | "ISOWEEKNUM" | "LEN" | "FIND" | "SEARCH" | "COUNT" | "COUNTA"
                | "COUNTBLANK" | "NETWORKDAYS" | "DATEDIF" | "INT" | "CODE" | "QUOTIENT"
                | "EVEN" | "ODD" => K::Integer,
                "AND" | "OR" | "NOT" | "XOR" | "TRUE" | "FALSE" | "EXACT" | "ISBLANK"
                | "ISNUMBER" | "ISTEXT" | "ISLOGICAL" | "ISERROR" | "ISERR" | "ISNA" | "ISEVEN"
                | "ISODD" => K::Bool,
                "LEFT" | "RIGHT" | "MID" | "UPPER" | "LOWER" | "PROPER" | "TRIM" | "CLEAN"
                | "CONCAT" | "CONCATENATE" | "TEXTJOIN" | "SUBSTITUTE" | "REPLACE" | "REPT"
                | "TEXT" | "FIXED" | "CHAR" | "T" | "TEXTBEFORE" | "TEXTAFTER" => K::Text,
                _ => K::Number,
            },
        }
    }

    /// Read output cell `col` of `row` as a formula value.
    fn read(&self, row: &[CellValue], col: usize) -> Value {
        let Some(cell) = row.get(col) else {
            return Value::Blank;
        };
        let kind = match self.layout.get(col) {
            Some(Slot::Source(i)) => self.source_kinds[*i],
            Some(Slot::Computed(k)) => match self.kinds[*k] {
                ResultKind::Date | ResultKind::DateTime => SourceKind::Date,
                ResultKind::Number | ResultKind::Integer => SourceKind::Number,
                _ => SourceKind::Other,
            },
            None => SourceKind::Other,
        };
        match cell {
            CellValue::Null => Value::Blank,
            CellValue::Bool(b) => Value::Bool(*b),
            CellValue::I64(n) => Value::Number(*n as f64),
            CellValue::F64(n) => Value::Number(*n),
            CellValue::Bytes(b) => Value::Text(format!(
                "0x{}",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            )),
            CellValue::String(s) => {
                if let Some(e) = parse_error_text(s) {
                    return Value::Error(e);
                }
                match kind {
                    SourceKind::Date => formula::parse_datetime_text(s)
                        .map(Value::Number)
                        .unwrap_or_else(|| Value::Text(s.clone())),
                    SourceKind::Number => s
                        .trim()
                        .parse::<f64>()
                        .map(Value::Number)
                        .unwrap_or_else(|_| Value::Text(s.clone())),
                    SourceKind::Other => Value::Text(s.clone()),
                }
            }
        }
    }

    /// Extend `rows` (source-width rows, in arrival order) into output rows,
    /// computing every formula. `earlier` are the output rows already
    /// computed, which `[n]` references read.
    pub fn extend_rows(
        &self,
        earlier: &[Vec<CellValue>],
        rows: Vec<Vec<CellValue>>,
        variables: &[(String, Value)],
        now: f64,
    ) -> Vec<Vec<CellValue>> {
        self.extend_rows_from(earlier.len(), earlier, rows, variables, now)
    }

    /// [`Self::extend_rows`] for rows starting at result row `base`, with
    /// `earlier` holding the output rows just before it (possibly fewer than
    /// `base` — rows before those read as blank).
    pub fn extend_rows_from(
        &self,
        base: usize,
        earlier: &[Vec<CellValue>],
        rows: Vec<Vec<CellValue>>,
        variables: &[(String, Value)],
        now: f64,
    ) -> Vec<Vec<CellValue>> {
        let earlier_start = base.saturating_sub(earlier.len());
        let mut fresh: Vec<Vec<CellValue>> = Vec::with_capacity(rows.len());
        for mut source in rows {
            source.resize(self.source_width, CellValue::Null);
            let mut out: Vec<CellValue> = self
                .layout
                .iter()
                .map(|slot| match slot {
                    Slot::Source(i) => std::mem::replace(&mut source[*i], CellValue::Null),
                    Slot::Computed(_) => CellValue::Null,
                })
                .collect();
            let row_index = (base + fresh.len()) as i64;
            for k in 0..self.columns.len() {
                let value = {
                    let cell = |name: &str, r: i64| -> Option<Value> {
                        let col = self
                            .names
                            .iter()
                            .position(|n| n.eq_ignore_ascii_case(name))?;
                        if r == row_index {
                            return Some(self.read(&out, col));
                        }
                        let r = usize::try_from(r).ok();
                        let row = r.and_then(|r| {
                            if r < base {
                                r.checked_sub(earlier_start).and_then(|i| earlier.get(i))
                            } else {
                                fresh.get(r - base)
                            }
                        });
                        Some(row.map(|row| self.read(row, col)).unwrap_or(Value::Blank))
                    };
                    let between = |a: &str, b: &str| self.between(a, b);
                    let variable = |name: &str| {
                        variables
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case(name))
                            .map(|(_, v)| v.clone())
                    };
                    let env = EvalEnv {
                        row: row_index,
                        cell: &cell,
                        columns_between: &between,
                        variable: &variable,
                        now,
                    };
                    formula::evaluate(&self.columns[k].formula.expr, &env)
                };
                let pos = self
                    .layout
                    .iter()
                    .position(|s| *s == Slot::Computed(k))
                    .expect("planned");
                out[pos] = to_cell(&value, self.kinds[k]);
            }
            fresh.push(out);
        }
        fresh
    }
}

impl ComputedPlan {
    /// How many rows back any formula reads (`[n]`, ranges), capped — the
    /// look-behind a paged window computes before its first row.
    pub fn max_lookback(&self) -> usize {
        self.columns
            .iter()
            .flat_map(|c| c.formula.refs.iter())
            .filter_map(|r| match r.row {
                formula::RowRef::Relative(n) if n > 0 => usize::try_from(n).ok(),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .min(MAX_WINDOW_LOOKBACK)
    }
}

/// Look-behind cap for paged results.
const MAX_WINDOW_LOOKBACK: usize = 1000;

/// A result's computed columns plus the run's variables and clock — what a
/// spill-file reader needs to produce full rows on its own.
#[derive(Debug, Clone)]
pub struct ComputedWindow {
    pub plan: ComputedPlan,
    pub variables: Vec<(String, Value)>,
    pub now: f64,
}

impl ComputedWindow {
    /// Rows `[offset, offset + len)` of a result too large to keep in
    /// memory: the source rows (and up to [`ComputedPlan::max_lookback`]
    /// before them) read through `read`, extended with the computed columns.
    /// A running formula only sees the rows read with the window.
    pub fn page(
        &self,
        offset: usize,
        len: usize,
        read: impl FnOnce(usize, usize) -> Vec<Vec<CellValue>>,
    ) -> Vec<Vec<CellValue>> {
        let start = offset.saturating_sub(self.plan.max_lookback());
        let raw = read(start, len + (offset - start));
        let mut rows = self
            .plan
            .extend_rows_from(start, &[], raw, &self.variables, self.now);
        let skip = (offset - start).min(rows.len());
        rows.split_off(skip)
    }
}

/// Spill files whose readers add computed columns, by path. Registered by
/// the results view when a result spills, read by every reader of the file
/// (the grid's pager, background scans, streamed exports), so they all see
/// the same columns.
static SPILL_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<ComputedWindow>>>,
> = std::sync::LazyLock::new(Default::default);

pub fn register_spill(path: &str, window: ComputedWindow) {
    if let Ok(mut map) = SPILL_REGISTRY.lock() {
        map.insert(path.to_string(), std::sync::Arc::new(window));
    }
}

pub fn spill_window(path: &str) -> Option<std::sync::Arc<ComputedWindow>> {
    SPILL_REGISTRY.lock().ok()?.get(path).cloned()
}

pub fn forget_spill(path: &str) {
    if let Ok(mut map) = SPILL_REGISTRY.lock() {
        map.remove(path);
    }
}

/// The kind of a column whose value comes from one of several branches: a
/// shared kind stays, numbers mixed with whole numbers are numbers, dates
/// mixed with date-times are date-times, anything else is text.
fn combine_kinds(kinds: &[ResultKind]) -> ResultKind {
    use ResultKind as K;
    let Some(first) = kinds.first() else {
        return K::Text;
    };
    if kinds.iter().all(|k| k == first) {
        return *first;
    }
    if kinds.iter().all(|k| matches!(k, K::Integer | K::Number)) {
        return K::Number;
    }
    if kinds.iter().all(|k| matches!(k, K::Date | K::DateTime)) {
        return K::DateTime;
    }
    K::Text
}

fn parse_error_text(s: &str) -> Option<formula::ErrorKind> {
    if !s.starts_with('#') {
        return None;
    }
    use formula::ErrorKind as E;
    [E::Null, E::Div0, E::Value, E::Ref, E::Name, E::Num, E::NA]
        .into_iter()
        .find(|e| e.as_str() == s)
}

/// A formula value as a grid cell of the column's kind.
fn to_cell(value: &Value, kind: ResultKind) -> CellValue {
    match value {
        Value::Blank => CellValue::Null,
        Value::Bool(b) => CellValue::Bool(*b),
        Value::Text(t) => CellValue::String(t.clone()),
        Value::Error(e) => CellValue::String(e.as_str().to_string()),
        Value::Array(_) => CellValue::String(formula::ErrorKind::Value.as_str().to_string()),
        Value::Number(n) => match kind {
            ResultKind::Date | ResultKind::DateTime => match formula::datetime_from_serial(*n) {
                Some(dt) if kind == ResultKind::Date && n.fract() == 0.0 => {
                    CellValue::String(dt.format("%Y-%m-%d").to_string())
                }
                Some(dt) => CellValue::String(dt.format("%Y-%m-%d %H:%M:%S").to_string()),
                None => CellValue::F64(*n),
            },
            ResultKind::Integer if n.fract() == 0.0 && n.abs() < 9.0e15 => {
                CellValue::I64(*n as i64)
            }
            _ => CellValue::F64(*n),
        },
    }
}

/// The `@name` values a run's formulas read: `-- sqlly set` variables (as
/// numbers when they read as one), then `-- sqlly param:` values as typed
/// (the strip's value, else the declared default).
pub fn formula_variables(
    full_buffer: &str,
    param_values: &[(String, String)],
) -> Vec<(String, Value)> {
    use crate::query_params::ParamType;
    let mut out: Vec<(String, Value)> = Vec::new();
    for var in crate::query_params::parse_set_directives(full_buffer) {
        let value = var
            .value
            .trim()
            .parse::<f64>()
            .map(Value::Number)
            .unwrap_or_else(|_| Value::Text(var.value.clone()));
        out.push((var.name, value));
    }
    for spec in crate::query_params::parse_param_directives(full_buffer) {
        let supplied = param_values
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&spec.name))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
            .or_else(|| spec.default.clone());
        let Some(raw) = supplied else {
            continue;
        };
        let raw = raw.trim();
        let value = if raw.eq_ignore_ascii_case("null") {
            Value::Blank
        } else {
            match spec.ty {
                ParamType::Int | ParamType::Decimal => raw
                    .parse::<f64>()
                    .map(Value::Number)
                    .unwrap_or_else(|_| Value::Text(raw.to_string())),
                ParamType::Bool => Value::Bool(matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes"
                )),
                ParamType::Date | ParamType::DateTime | ParamType::Time => {
                    formula::parse_datetime_text(raw)
                        .map(Value::Number)
                        .unwrap_or_else(|| Value::Text(raw.to_string()))
                }
                _ => Value::Text(raw.to_string()),
            }
        };
        out.push((spec.name.trim_start_matches('@').to_string(), value));
    }
    out
}

/// TODAY()/NOW() for a run: the local wall clock as a serial number.
pub fn now_serial() -> f64 {
    formula::serial_from_datetime(chrono::Local::now().naive_local())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
        }
    }

    #[test]
    fn parses_name_formula_and_options() {
        let c = parse_body(
            " Margin % = IF(!Revenue=0,\"a;b\",(!Revenue-!Cost)/!Revenue); after=Cost; result=2",
        )
        .expect("parses");
        assert_eq!(c.name, "Margin %");
        assert_eq!(c.after.as_deref(), Some("Cost"));
        assert_eq!(c.result, Some(2));
        assert_eq!(c.formula.referenced_columns(), vec!["Revenue", "Cost"]);
        assert!(parse_body("= 1").is_err());
        assert!(parse_body("X = 1 +").is_err());
        assert!(parse_body("X = 1; wat=2").is_err());
        let line = directive_line("Margin %", "=!A/!B", Some("B"));
        assert_eq!(line, "-- sqlly column: Margin % = !A/!B; after=B");
        assert_eq!(parse(&format!("{line}\nSELECT 1")).len(), 1);
    }

    #[test]
    fn computes_running_totals_and_places_columns() {
        let specs = parse(
            "-- sqlly column: Total = !Qty * !Price; after=Qty\n\
             -- sqlly column: Running = !Total + N(!Running[1])\n\
             -- sqlly column: Shipped = !Day + 2\n\
             -- sqlly column: Unused = !Nope * 2\nSELECT 1",
        );
        let source = vec![
            col("Qty", "int"),
            col("Price", "decimal(10,2)"),
            col("Day", "date"),
        ];
        let plan = ComputedPlan::new(&specs, &source, 0).expect("plan");
        let names: Vec<String> = plan
            .output_columns(&source)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            vec!["Qty", "Total", "Price", "Day", "Running", "Shipped"]
        );
        let types: Vec<String> = plan
            .output_columns(&source)
            .into_iter()
            .map(|c| c.data_type)
            .collect();
        assert_eq!(types[1], "float");
        assert_eq!(types[5], "datetime2");
        let rows = [
            vec![
                CellValue::I64(2),
                CellValue::String("1.50".into()),
                CellValue::String("2026-10-23".into()),
            ],
            vec![CellValue::I64(3), CellValue::F64(2.0), CellValue::Null],
        ];
        let first = plan.extend_rows(&[], rows[..1].to_vec(), &[], 0.0);
        let second = plan.extend_rows(&first, rows[1..].to_vec(), &[], 0.0);
        assert_eq!(first[0][1], CellValue::F64(3.0));
        assert_eq!(first[0][4], CellValue::F64(3.0));
        assert_eq!(first[0][5], CellValue::String("2026-10-25 00:00:00".into()));
        assert_eq!(second[0][1], CellValue::F64(6.0));
        assert_eq!(
            second[0][4],
            CellValue::F64(9.0),
            "running total reads the row before"
        );
        assert_eq!(plan.output_index_of_source(1), Some(2));
    }

    #[test]
    fn errors_show_as_excel_errors_and_variables_resolve() {
        let specs = parse("-- sqlly column: Ratio = !A / !B * @scale\nSELECT 1");
        let source = vec![col("A", "int"), col("B", "int")];
        let plan = ComputedPlan::new(&specs, &source, 0).unwrap();
        let rows = plan.extend_rows(
            &[],
            vec![
                vec![CellValue::I64(1), CellValue::I64(0)],
                vec![CellValue::I64(1), CellValue::I64(4)],
            ],
            &[("scale".into(), Value::Number(10.0))],
            0.0,
        );
        assert_eq!(rows[0][2], CellValue::String("#DIV/0!".into()));
        assert_eq!(rows[1][2], CellValue::F64(2.5));
    }

    #[test]
    fn branch_kinds_combine_like_a_spreadsheet_column() {
        let specs = parse(
            "-- sqlly column: A = IF(ISBLANK(!D), 0, DAYS(!D, !D))\n\
             -- sqlly column: B = IF(!N > 1, 0.5, 2)\n\
             -- sqlly column: C = IF(!N > 1, \"big\", 2)\nSELECT 1",
        );
        let source = vec![col("D", "date"), col("N", "int")];
        let plan = ComputedPlan::new(&specs, &source, 0).unwrap();
        let types: Vec<String> = plan
            .output_columns(&source)
            .into_iter()
            .map(|c| c.data_type)
            .collect();
        assert_eq!(types[2..], ["int", "float", "nvarchar"]);
    }

    #[test]
    fn upsert_replaces_the_named_line_or_adds_one() {
        let sql = "-- sqlly column: A = 1\nSELECT 1;";
        let (text, _) = upsert_line(sql, (0, sql.len()), "-- sqlly column: B = 2", Some("A"));
        assert_eq!(text, "-- sqlly column: B = 2\nSELECT 1;");
        let (text, span) = upsert_line(&text, (0, text.len()), "-- sqlly column: C = 3", None);
        assert_eq!(
            text,
            "-- sqlly column: C = 3\n-- sqlly column: B = 2\nSELECT 1;"
        );
        assert_eq!(span, (0, text.len()));
    }

    #[test]
    fn variables_come_from_set_and_param_directives() {
        let vars = formula_variables(
            "-- sqlly set region = West\n-- sqlly set rate = 0.2\n-- sqlly param: @since date = 2026-01-01\n-- sqlly param: @n int\n",
            &[("@n".into(), "7".into())],
        );
        assert!(vars.contains(&("region".into(), Value::Text("West".into()))));
        assert!(vars.contains(&("rate".into(), Value::Number(0.2))));
        assert!(vars.contains(&("n".into(), Value::Number(7.0))));
        assert!(
            vars.iter()
                .any(|(n, v)| n == "since" && matches!(v, Value::Number(_)))
        );
    }
}
