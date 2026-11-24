//! Compare two notebook outputs: schema changes, then rows aligned by key,
//! by position, or as multisets, with numeric tolerance and text
//! normalization. Counts are always exact; row detail is capped.
//!
//! Strict typed comparison of two run snapshots (the results pane's run
//! difference) is [`super::resultset`]; table-to-table compares streamed in
//! key order from two servers are `crate::datacompare`. This wrapper is the
//! one notebook diff cells call: it accepts differently shaped outputs and
//! reports what changed in the schema instead of refusing them.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::decimal::Decimal;
use super::resultset::cell_equal;
use super::value::{StableHasher, cell_text, format_float, numeric_literal};
use super::{Completeness, OutputView, column_index};

/// How rows on the two sides are paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowAlignment {
    /// Pair rows by the named key columns (left-side names; the column map
    /// applies). Keys must be unique and non-NULL on both sides.
    Keyed { columns: Vec<String> },
    /// Pair row *i* with row *i*.
    Positional,
    /// Order-insensitive: identical rows cancel out, the rest are added or
    /// removed.
    Multiset,
}

/// Numeric closeness: two numbers match when `|a − b| <= absolute` or
/// `|a − b| <= relative × max(|a|, |b|)`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Tolerance {
    #[serde(default)]
    pub absolute: f64,
    #[serde(default)]
    pub relative: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffOptions {
    pub alignment: RowAlignment,
    /// Left column name → right column name, for renamed columns. Unmapped
    /// columns pair by name (exact, then case-insensitive).
    pub column_map: BTreeMap<String, String>,
    /// Left-side column names left out of the comparison.
    pub ignore_columns: Vec<String>,
    /// `None` compares numbers exactly (by value, so `1` equals `1.0`).
    pub numeric_tolerance: Option<Tolerance>,
    /// Compare numeric text (decimals delivered as strings) as numbers.
    pub numeric_text: bool,
    pub case_insensitive: bool,
    /// Trim text and collapse inner whitespace runs before comparing.
    pub normalize_whitespace: bool,
    /// Cap on each detail list (added, removed, changed); counts stay exact.
    pub max_reported_rows: usize,
    /// Rows read per side; a larger side is refused.
    pub max_input_rows: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            alignment: RowAlignment::Multiset,
            column_map: BTreeMap::new(),
            ignore_columns: Vec::new(),
            numeric_tolerance: None,
            numeric_text: true,
            case_insensitive: false,
            normalize_whitespace: false,
            max_reported_rows: 500,
            max_input_rows: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeChange {
    pub left_column: String,
    pub right_column: String,
    pub left_type: String,
    pub right_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SchemaDiff {
    /// Right-side columns with no left counterpart.
    pub added_columns: Vec<String>,
    /// Left-side columns with no right counterpart.
    pub removed_columns: Vec<String>,
    pub type_changes: Vec<TypeChange>,
    /// Paired columns whose names differ (through the column map or case).
    pub renamed: Vec<(String, String)>,
    /// The paired columns appear in a different order.
    pub reordered: bool,
    /// Left-side names of the columns whose values were compared.
    pub compared_columns: Vec<String>,
}

impl SchemaDiff {
    pub fn is_unchanged(&self) -> bool {
        self.added_columns.is_empty()
            && self.removed_columns.is_empty()
            && self.type_changes.is_empty()
            && self.renamed.is_empty()
            && !self.reordered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    pub left_rows: u64,
    pub right_rows: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub unchanged: u64,
}

impl DiffSummary {
    pub fn is_identical(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.changed == 0
    }
}

/// A row present on one side only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffRow {
    /// 0-based row index on its own side.
    pub index: usize,
    pub values: Vec<CellValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellChange {
    /// Left-side column name.
    pub column: String,
    pub left: CellValue,
    pub right: CellValue,
    /// right − left, when both are numbers.
    pub numeric_delta: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangedDiffRow {
    pub left_index: usize,
    pub right_index: usize,
    /// Key values (keyed alignment) or empty (positional).
    pub key: Vec<CellValue>,
    pub changes: Vec<CellChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputDiff {
    pub summary: DiffSummary,
    pub schema: SchemaDiff,
    pub added: Vec<DiffRow>,
    pub removed: Vec<DiffRow>,
    pub changed: Vec<ChangedDiffRow>,
    /// Changed-cell count per compared column (left names).
    pub column_change_counts: BTreeMap<String, u64>,
    /// Some detail rows were left out by `max_reported_rows`.
    pub detail_truncated: bool,
    pub left_completeness: Completeness,
    pub right_completeness: Completeness,
}

impl OutputDiff {
    /// Whether both inputs were complete, so the counts describe the whole
    /// results rather than the rows that were read.
    pub fn is_conclusive(&self) -> bool {
        self.left_completeness.is_complete() && self.right_completeness.is_complete()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputDiffError {
    /// A side holds more rows than `max_input_rows`.
    TooManyRows {
        side: DiffSideName,
        limit: usize,
    },
    /// A key column (left name) is missing on a side.
    MissingKeyColumn {
        side: DiffSideName,
        column: String,
    },
    NullKey {
        side: DiffSideName,
        row: usize,
    },
    DuplicateKey {
        side: DiffSideName,
        row: usize,
    },
    /// No column pairs up between the two sides.
    NoCommonColumns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSideName {
    Left,
    Right,
}

/// Compare `left` (before) with `right` (after).
pub fn diff_outputs(
    left: &OutputView<'_>,
    right: &OutputView<'_>,
    options: &DiffOptions,
) -> Result<OutputDiff, OutputDiffError> {
    let pairs = pair_columns(left, right, options);
    let schema = schema_diff(left, right, &pairs);
    let compared: Vec<(usize, usize)> = pairs
        .iter()
        .filter(|(l, _)| {
            !options
                .ignore_columns
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&left.columns[*l].name))
        })
        .copied()
        .collect();
    if compared.is_empty() && !(left.columns.is_empty() && right.columns.is_empty()) {
        return Err(OutputDiffError::NoCommonColumns);
    }
    let left_rows =
        left.rows_up_to(options.max_input_rows)
            .ok_or(OutputDiffError::TooManyRows {
                side: DiffSideName::Left,
                limit: options.max_input_rows,
            })?;
    let right_rows =
        right
            .rows_up_to(options.max_input_rows)
            .ok_or(OutputDiffError::TooManyRows {
                side: DiffSideName::Right,
                limit: options.max_input_rows,
            })?;

    let comparer = Comparer {
        options,
        compared: &compared,
        names: compared
            .iter()
            .map(|(l, _)| left.columns[*l].name.clone())
            .collect(),
    };
    let mut builder = DiffBuilder::new(options.max_reported_rows, &comparer.names);
    match &options.alignment {
        RowAlignment::Positional => {
            let common = left_rows.len().min(right_rows.len());
            for index in 0..common {
                builder.compare_pair(
                    &comparer,
                    index,
                    index,
                    Vec::new(),
                    &left_rows[index],
                    &right_rows[index],
                );
            }
            for (index, row) in left_rows.iter().enumerate().skip(common) {
                builder.removed(index, row);
            }
            for (index, row) in right_rows.iter().enumerate().skip(common) {
                builder.added(index, row);
            }
        }
        RowAlignment::Multiset => {
            let mut right_by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
            for (index, row) in right_rows.iter().enumerate() {
                right_by_hash
                    .entry(comparer.row_hash(row, Side::Right))
                    .or_default()
                    .push(index);
            }
            let mut matched = vec![false; right_rows.len()];
            for (left_index, row) in left_rows.iter().enumerate() {
                let found = right_by_hash
                    .get_mut(&comparer.row_hash(row, Side::Left))
                    .and_then(|bucket| {
                        let position = bucket.iter().position(|right_index| {
                            !matched[*right_index]
                                && comparer.rows_match(row, &right_rows[*right_index])
                        })?;
                        Some(bucket.remove(position))
                    });
                match found {
                    Some(right_index) => {
                        matched[right_index] = true;
                        builder.summary.unchanged += 1;
                    }
                    None => builder.removed(left_index, row),
                }
            }
            for (index, row) in right_rows.iter().enumerate() {
                if !matched[index] {
                    builder.added(index, row);
                }
            }
        }
        RowAlignment::Keyed { columns } => {
            let mut keys = Vec::with_capacity(columns.len());
            for name in columns {
                let left_key = column_index(left.columns, name).ok_or_else(|| {
                    OutputDiffError::MissingKeyColumn {
                        side: DiffSideName::Left,
                        column: name.clone(),
                    }
                })?;
                let right_key = pairs
                    .iter()
                    .find(|(l, _)| *l == left_key)
                    .map(|(_, r)| *r)
                    .ok_or_else(|| OutputDiffError::MissingKeyColumn {
                        side: DiffSideName::Right,
                        column: name.clone(),
                    })?;
                keys.push((left_key, right_key));
            }
            let right_index = comparer.key_index(&right_rows, &keys, Side::Right)?;
            comparer.key_index(&left_rows, &keys, Side::Left)?;
            let mut matched = vec![false; right_rows.len()];
            for (index, row) in left_rows.iter().enumerate() {
                match comparer.find_key(row, &keys, &right_index, &right_rows) {
                    Some(other) => {
                        matched[other] = true;
                        let key = keys.iter().map(|(l, _)| row[*l].clone()).collect();
                        builder.compare_pair(&comparer, index, other, key, row, &right_rows[other]);
                    }
                    None => builder.removed(index, row),
                }
            }
            for (index, row) in right_rows.iter().enumerate() {
                if !matched[index] {
                    builder.added(index, row);
                }
            }
        }
    }
    builder.summary.left_rows = left_rows.len() as u64;
    builder.summary.right_rows = right_rows.len() as u64;
    Ok(OutputDiff {
        summary: builder.summary,
        schema,
        added: builder.added,
        removed: builder.removed,
        changed: builder.changed,
        column_change_counts: builder.column_counts,
        detail_truncated: builder.truncated,
        left_completeness: left.completeness,
        right_completeness: right.completeness,
    })
}

fn pair_columns(
    left: &OutputView<'_>,
    right: &OutputView<'_>,
    options: &DiffOptions,
) -> Vec<(usize, usize)> {
    let mut used = vec![false; right.columns.len()];
    let mut pairs = Vec::new();
    for (l, column) in left.columns.iter().enumerate() {
        let target = options
            .column_map
            .get(&column.name)
            .map(String::as_str)
            .unwrap_or(&column.name);
        let exact = right
            .columns
            .iter()
            .enumerate()
            .position(|(r, c)| !used[r] && c.name == target);
        let found = exact.or_else(|| {
            right
                .columns
                .iter()
                .enumerate()
                .position(|(r, c)| !used[r] && c.name.eq_ignore_ascii_case(target))
        });
        if let Some(r) = found {
            used[r] = true;
            pairs.push((l, r));
        }
    }
    pairs
}

fn schema_diff(
    left: &OutputView<'_>,
    right: &OutputView<'_>,
    pairs: &[(usize, usize)],
) -> SchemaDiff {
    let mut schema = SchemaDiff::default();
    for (l, column) in left.columns.iter().enumerate() {
        if !pairs.iter().any(|(pl, _)| *pl == l) {
            schema.removed_columns.push(column.name.clone());
        }
    }
    for (r, column) in right.columns.iter().enumerate() {
        if !pairs.iter().any(|(_, pr)| *pr == r) {
            schema.added_columns.push(column.name.clone());
        }
    }
    for (l, r) in pairs {
        let (lc, rc) = (&left.columns[*l], &right.columns[*r]);
        if lc.name != rc.name {
            schema.renamed.push((lc.name.clone(), rc.name.clone()));
        }
        if !lc.data_type.eq_ignore_ascii_case(&rc.data_type) {
            schema.type_changes.push(TypeChange {
                left_column: lc.name.clone(),
                right_column: rc.name.clone(),
                left_type: lc.data_type.clone(),
                right_type: rc.data_type.clone(),
            });
        }
        schema.compared_columns.push(lc.name.clone());
    }
    schema.reordered = pairs.windows(2).any(|w| w[0].1 > w[1].1);
    schema
}

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

struct Comparer<'a> {
    options: &'a DiffOptions,
    compared: &'a [(usize, usize)],
    names: Vec<String>,
}

/// A cell reduced to what the options compare. Integers and decimal text
/// stay exact, so values past a float's 15–17 digits still differ.
enum Normal<'c> {
    Null,
    Exact(Decimal),
    Float(f64),
    Text(std::borrow::Cow<'c, str>),
    Raw(&'c CellValue),
}

impl Comparer<'_> {
    fn normal<'c>(&self, cell: &'c CellValue) -> Normal<'c> {
        match cell {
            CellValue::Null => Normal::Null,
            CellValue::I64(n) => Normal::Exact(Decimal::from_i64(*n)),
            CellValue::F64(f) => Normal::Float(*f),
            CellValue::String(text) => {
                if self.options.numeric_text {
                    if let Some(exact) = Decimal::parse(text) {
                        return Normal::Exact(exact);
                    }
                    if let Some(number) = numeric_literal(text) {
                        return Normal::Float(number);
                    }
                }
                let mut text: std::borrow::Cow<'c, str> = std::borrow::Cow::Borrowed(text);
                if self.options.normalize_whitespace {
                    text = std::borrow::Cow::Owned(
                        text.split_whitespace().collect::<Vec<_>>().join(" "),
                    );
                }
                if self.options.case_insensitive {
                    text = std::borrow::Cow::Owned(text.to_lowercase());
                }
                Normal::Text(text)
            }
            CellValue::Bool(_) | CellValue::Bytes(_) => Normal::Raw(cell),
        }
    }

    fn cells_match(&self, a: &CellValue, b: &CellValue) -> bool {
        match (self.normal(a), self.normal(b)) {
            (Normal::Null, Normal::Null) => true,
            (Normal::Exact(x), Normal::Exact(y)) => {
                x == y
                    || self.options.numeric_tolerance.is_some_and(|tolerance| {
                        numbers_match(x.to_f64(), y.to_f64(), Some(tolerance))
                    })
            }
            (Normal::Exact(x), Normal::Float(y)) | (Normal::Float(y), Normal::Exact(x)) => {
                match self.options.numeric_tolerance {
                    None => Decimal::from_f64(y).is_some_and(|y| y == x),
                    tolerance => numbers_match(x.to_f64(), y, tolerance),
                }
            }
            (Normal::Float(x), Normal::Float(y)) => {
                numbers_match(x, y, self.options.numeric_tolerance)
            }
            (Normal::Text(x), Normal::Text(y)) => x == y,
            (Normal::Raw(x), Normal::Raw(y)) => cell_equal(x, y),
            _ => false,
        }
    }

    fn rows_match(&self, left: &[CellValue], right: &[CellValue]) -> bool {
        self.compared.iter().all(|(l, r)| {
            self.cells_match(
                left.get(*l).unwrap_or(&CellValue::Null),
                right.get(*r).unwrap_or(&CellValue::Null),
            )
        })
    }

    /// A hash consistent with [`Self::cells_match`] when no tolerance is
    /// set; with a tolerance every number hashes alike so close values can
    /// still meet in one bucket.
    fn cell_hash_into(&self, cell: &CellValue, hasher: &mut StableHasher) {
        match self.normal(cell) {
            Normal::Null => hasher.write(&[0]),
            Normal::Exact(value) => {
                hasher.write(&[1]);
                if self.options.numeric_tolerance.is_none() {
                    hash_exact(value, hasher);
                }
            }
            Normal::Float(value) => {
                hasher.write(&[1]);
                if self.options.numeric_tolerance.is_none() {
                    match Decimal::from_f64(value) {
                        Some(exact) => hash_exact(exact, hasher),
                        None => {
                            let bits = if value.is_nan() {
                                f64::NAN.to_bits()
                            } else {
                                value.to_bits()
                            };
                            hasher.write(&[9]);
                            hasher.write(&bits.to_le_bytes());
                        }
                    }
                }
            }
            Normal::Text(text) => {
                hasher.write(&[2]);
                hasher.write(&(text.len() as u64).to_le_bytes());
                hasher.write(text.as_bytes());
            }
            Normal::Raw(raw) => {
                hasher.write(&[3]);
                hasher.write(&super::value::cell_hash(raw, 0).to_le_bytes());
            }
        }
    }

    fn row_hash(&self, row: &[CellValue], side: Side) -> u64 {
        let mut hasher = StableHasher::new(0);
        for (l, r) in self.compared {
            let index = match side {
                Side::Left => *l,
                Side::Right => *r,
            };
            self.cell_hash_into(row.get(index).unwrap_or(&CellValue::Null), &mut hasher);
        }
        hasher.finish()
    }

    fn key_hash(&self, row: &[CellValue], keys: &[(usize, usize)], side: Side) -> u64 {
        let mut hasher = StableHasher::new(1);
        for (l, r) in keys {
            let index = match side {
                Side::Left => *l,
                Side::Right => *r,
            };
            self.cell_hash_into(row.get(index).unwrap_or(&CellValue::Null), &mut hasher);
        }
        hasher.finish()
    }

    fn keys_match(
        &self,
        left: &[CellValue],
        right: &[CellValue],
        keys: &[(usize, usize)],
        sides: (Side, Side),
    ) -> bool {
        keys.iter().all(|(l, r)| {
            let pick = |row: &[CellValue], side: Side| match side {
                Side::Left => row.get(*l).cloned().unwrap_or(CellValue::Null),
                Side::Right => row.get(*r).cloned().unwrap_or(CellValue::Null),
            };
            self.cells_match(&pick(left, sides.0), &pick(right, sides.1))
        })
    }

    fn key_index(
        &self,
        rows: &[Vec<CellValue>],
        keys: &[(usize, usize)],
        side: Side,
    ) -> Result<HashMap<u64, Vec<usize>>, OutputDiffError> {
        let side_name = match side {
            Side::Left => DiffSideName::Left,
            Side::Right => DiffSideName::Right,
        };
        let mut index: HashMap<u64, Vec<usize>> = HashMap::with_capacity(rows.len());
        for (row_index, row) in rows.iter().enumerate() {
            let null_key = keys.iter().any(|(l, r)| {
                let column = match side {
                    Side::Left => *l,
                    Side::Right => *r,
                };
                matches!(row.get(column), None | Some(CellValue::Null))
            });
            if null_key {
                return Err(OutputDiffError::NullKey {
                    side: side_name,
                    row: row_index,
                });
            }
            let bucket = index.entry(self.key_hash(row, keys, side)).or_default();
            if bucket
                .iter()
                .any(|other| self.keys_match(&rows[*other], row, keys, (side, side)))
            {
                return Err(OutputDiffError::DuplicateKey {
                    side: side_name,
                    row: row_index,
                });
            }
            bucket.push(row_index);
        }
        Ok(index)
    }

    fn find_key(
        &self,
        left_row: &[CellValue],
        keys: &[(usize, usize)],
        right_index: &HashMap<u64, Vec<usize>>,
        right_rows: &[Vec<CellValue>],
    ) -> Option<usize> {
        right_index
            .get(&self.key_hash(left_row, keys, Side::Left))?
            .iter()
            .copied()
            .find(|candidate| {
                self.keys_match(
                    left_row,
                    &right_rows[*candidate],
                    keys,
                    (Side::Left, Side::Right),
                )
            })
    }
}

/// Equal exact values (`1.50` and `1.5`, `1` and `1.0`) hash alike.
fn hash_exact(value: Decimal, hasher: &mut StableHasher) {
    let value = value.normalized();
    hasher.write(&value.mantissa().to_le_bytes());
    hasher.write(&value.scale().to_le_bytes());
}

fn numbers_match(a: f64, b: f64, tolerance: Option<Tolerance>) -> bool {
    if a.is_nan() || b.is_nan() {
        return a.is_nan() && b.is_nan();
    }
    if a == b {
        return true;
    }
    let Some(tolerance) = tolerance else {
        return false;
    };
    if a.is_infinite() || b.is_infinite() {
        return false;
    }
    let difference = (a - b).abs();
    difference <= tolerance.absolute || difference <= tolerance.relative * a.abs().max(b.abs())
}

struct DiffBuilder {
    summary: DiffSummary,
    added: Vec<DiffRow>,
    removed: Vec<DiffRow>,
    changed: Vec<ChangedDiffRow>,
    column_counts: BTreeMap<String, u64>,
    cap: usize,
    truncated: bool,
}

impl DiffBuilder {
    fn new(cap: usize, names: &[String]) -> Self {
        Self {
            summary: DiffSummary::default(),
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
            column_counts: names.iter().map(|name| (name.clone(), 0)).collect(),
            cap,
            truncated: false,
        }
    }

    fn added(&mut self, index: usize, row: &[CellValue]) {
        self.summary.added += 1;
        if self.added.len() < self.cap {
            self.added.push(DiffRow {
                index,
                values: row.to_vec(),
            });
        } else {
            self.truncated = true;
        }
    }

    fn removed(&mut self, index: usize, row: &[CellValue]) {
        self.summary.removed += 1;
        if self.removed.len() < self.cap {
            self.removed.push(DiffRow {
                index,
                values: row.to_vec(),
            });
        } else {
            self.truncated = true;
        }
    }

    fn compare_pair(
        &mut self,
        comparer: &Comparer<'_>,
        left_index: usize,
        right_index: usize,
        key: Vec<CellValue>,
        left: &[CellValue],
        right: &[CellValue],
    ) {
        let mut changes = Vec::new();
        for ((l, r), name) in comparer.compared.iter().zip(&comparer.names) {
            let a = left.get(*l).unwrap_or(&CellValue::Null);
            let b = right.get(*r).unwrap_or(&CellValue::Null);
            if !comparer.cells_match(a, b) {
                *self.column_counts.entry(name.clone()).or_default() += 1;
                let numeric_delta = match (comparer.normal(a), comparer.normal(b)) {
                    (Normal::Exact(x), Normal::Exact(y)) => Some(y.to_f64() - x.to_f64()),
                    (Normal::Exact(x), Normal::Float(y)) => Some(y - x.to_f64()),
                    (Normal::Float(x), Normal::Exact(y)) => Some(y.to_f64() - x),
                    (Normal::Float(x), Normal::Float(y)) => Some(y - x),
                    _ => None,
                };
                changes.push(CellChange {
                    column: name.clone(),
                    left: a.clone(),
                    right: b.clone(),
                    numeric_delta,
                });
            }
        }
        if changes.is_empty() {
            self.summary.unchanged += 1;
            return;
        }
        self.summary.changed += 1;
        if self.changed.len() < self.cap {
            self.changed.push(ChangedDiffRow {
                left_index,
                right_index,
                key,
                changes,
            });
        } else {
            self.truncated = true;
        }
    }
}

/// Display text for a changed cell, as the diff renders it.
pub fn change_text(cell: &CellValue) -> String {
    match cell {
        CellValue::F64(value) => format_float(*value),
        other => cell_text(other)
            .map(|t| t.into_owned())
            .unwrap_or_else(|| "NULL".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::*;
    use sqlly_core::ResultColumn;

    fn view<'a>(columns: &'a [ResultColumn], rows: &'a [Vec<CellValue>]) -> OutputView<'a> {
        OutputView::complete(columns, rows)
    }

    fn keyed(names: &[&str]) -> DiffOptions {
        DiffOptions {
            alignment: RowAlignment::Keyed {
                columns: names.iter().map(|n| n.to_string()).collect(),
            },
            ..DiffOptions::default()
        }
    }

    #[test]
    fn keyed_diff_reports_added_removed_changed() {
        let cols = vec![col("id", "int"), col("amount", "decimal")];
        let left = vec![
            vec![i(1), s("10.00")],
            vec![i(2), s("20.00")],
            vec![i(3), s("30")],
        ];
        let right = vec![
            vec![i(3), f(30.0)],
            vec![i(2), s("25.50")],
            vec![i(4), s("40")],
        ];
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &keyed(&["id"])).unwrap();
        assert_eq!(
            d.summary,
            DiffSummary {
                left_rows: 3,
                right_rows: 3,
                added: 1,
                removed: 1,
                changed: 1,
                unchanged: 1
            }
        );
        assert_eq!(d.removed[0].values[0], i(1));
        assert_eq!(d.added[0].values[0], i(4));
        assert_eq!(d.changed[0].key, vec![i(2)]);
        assert_eq!(d.changed[0].changes[0].numeric_delta, Some(5.5));
        assert_eq!(d.column_change_counts["amount"], 1);
        assert!(d.schema.is_unchanged());
        assert!(d.is_conclusive());
    }

    #[test]
    fn wide_decimals_and_bigints_compare_exactly() {
        let cols = vec![col("id", "bigint"), col("amount", "decimal(38,2)")];
        let left = vec![
            vec![i(1), s("12345678901234567.89")],
            vec![i(2), i(9007199254740993)],
        ];
        let right = vec![
            vec![i(1), s("12345678901234567.88")],
            vec![i(2), i(9007199254740992)],
        ];
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &keyed(&["id"])).unwrap();
        assert_eq!(d.summary.changed, 2);
        assert_eq!(d.summary.unchanged, 0);

        let keys = vec![col("id", "bigint")];
        let left = vec![vec![i(1234567890123456768)]];
        let right = vec![vec![i(1234567890123456789)]];
        let d = diff_outputs(&view(&keys, &left), &view(&keys, &right), &keyed(&["id"])).unwrap();
        assert_eq!((d.summary.added, d.summary.removed), (1, 1));

        let left = vec![vec![i(1), s("1.50")], vec![i(2), f(3.0)]];
        let right = vec![vec![i(1), s("1.5")], vec![i(2), i(3)]];
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &keyed(&["id"])).unwrap();
        assert_eq!(
            d.summary.unchanged, 2,
            "equal values with different spellings match"
        );
    }

    #[test]
    fn tolerance_case_and_whitespace_options() {
        let cols = vec![col("k", "text"), col("x", "float"), col("name", "text")];
        let left = vec![vec![s("a"), f(1.0), s("Hello  World")]];
        let right = vec![vec![s("a"), f(1.004), s(" hello world ")]];
        let strict =
            diff_outputs(&view(&cols, &left), &view(&cols, &right), &keyed(&["k"])).unwrap();
        assert_eq!(strict.summary.changed, 1);
        assert_eq!(strict.changed[0].changes.len(), 2);
        let relaxed = DiffOptions {
            numeric_tolerance: Some(Tolerance {
                absolute: 0.01,
                relative: 0.0,
            }),
            case_insensitive: true,
            normalize_whitespace: true,
            ..keyed(&["k"])
        };
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &relaxed).unwrap();
        assert!(d.summary.is_identical(), "{d:?}");
        let relative = DiffOptions {
            numeric_tolerance: Some(Tolerance {
                absolute: 0.0,
                relative: 0.001,
            }),
            ..relaxed.clone()
        };
        assert_eq!(
            diff_outputs(&view(&cols, &left), &view(&cols, &right), &relative)
                .unwrap()
                .summary
                .changed,
            1
        );
    }

    #[test]
    fn multiset_and_positional_alignment() {
        let cols = vec![col("v", "int")];
        let left = vec![vec![i(1)], vec![i(1)], vec![i(2)]];
        let right = vec![vec![i(2)], vec![i(1)], vec![i(3)]];
        let multiset = diff_outputs(
            &view(&cols, &left),
            &view(&cols, &right),
            &DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(
            (
                multiset.summary.added,
                multiset.summary.removed,
                multiset.summary.unchanged
            ),
            (1, 1, 2)
        );
        let positional = DiffOptions {
            alignment: RowAlignment::Positional,
            ..DiffOptions::default()
        };
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right[..2]), &positional).unwrap();
        assert_eq!(
            (d.summary.changed, d.summary.unchanged, d.summary.removed),
            (1, 1, 1)
        );
        assert_eq!(d.removed[0].index, 2);
    }

    #[test]
    fn multiset_with_tolerance_pairs_close_numbers() {
        let cols = vec![col("v", "float")];
        let left = vec![vec![f(1.0)], vec![f(2.0)]];
        let right = vec![vec![f(2.0000001)], vec![f(0.9999999)]];
        let options = DiffOptions {
            numeric_tolerance: Some(Tolerance {
                absolute: 1e-6,
                relative: 0.0,
            }),
            ..DiffOptions::default()
        };
        assert!(
            diff_outputs(&view(&cols, &left), &view(&cols, &right), &options)
                .unwrap()
                .summary
                .is_identical()
        );
    }

    #[test]
    fn schema_changes_are_reported_and_mapped_columns_compare() {
        let left_cols = vec![
            col("id", "int"),
            col("total", "int"),
            col("dropped", "text"),
        ];
        let right_cols = vec![
            col("Total_Amount", "bigint"),
            col("ID", "int"),
            col("new", "text"),
        ];
        let left = vec![vec![i(1), i(10), s("x")]];
        let right = vec![vec![i(11), i(1), s("y")]];
        let options = DiffOptions {
            column_map: BTreeMap::from([("total".to_string(), "Total_Amount".to_string())]),
            ..keyed(&["id"])
        };
        let d = diff_outputs(
            &view(&left_cols, &left),
            &view(&right_cols, &right),
            &options,
        )
        .unwrap();
        assert_eq!(d.schema.added_columns, vec!["new"]);
        assert_eq!(d.schema.removed_columns, vec!["dropped"]);
        assert_eq!(d.schema.type_changes.len(), 1);
        assert_eq!(d.schema.renamed.len(), 2);
        assert!(d.schema.reordered);
        assert_eq!(d.summary.changed, 1);
        assert_eq!(d.changed[0].changes[0].column, "total");
    }

    #[test]
    fn errors_for_bad_keys_and_oversized_inputs() {
        let cols = vec![col("id", "int")];
        let dup = vec![vec![i(1)], vec![i(1)]];
        let null = vec![vec![CellValue::Null]];
        let ok = vec![vec![i(1)]];
        assert_eq!(
            diff_outputs(&view(&cols, &dup), &view(&cols, &ok), &keyed(&["id"])).unwrap_err(),
            OutputDiffError::DuplicateKey {
                side: DiffSideName::Left,
                row: 1
            }
        );
        assert_eq!(
            diff_outputs(&view(&cols, &ok), &view(&cols, &null), &keyed(&["id"])).unwrap_err(),
            OutputDiffError::NullKey {
                side: DiffSideName::Right,
                row: 0
            }
        );
        assert!(matches!(
            diff_outputs(&view(&cols, &ok), &view(&cols, &ok), &keyed(&["nope"])).unwrap_err(),
            OutputDiffError::MissingKeyColumn {
                side: DiffSideName::Left,
                ..
            }
        ));
        let small = DiffOptions {
            max_input_rows: 1,
            ..DiffOptions::default()
        };
        assert_eq!(
            diff_outputs(&view(&cols, &dup), &view(&cols, &ok), &small).unwrap_err(),
            OutputDiffError::TooManyRows {
                side: DiffSideName::Left,
                limit: 1
            }
        );
        let other = vec![col("zzz", "int")];
        assert_eq!(
            diff_outputs(
                &view(&cols, &ok),
                &view(&other, &ok),
                &DiffOptions::default()
            )
            .unwrap_err(),
            OutputDiffError::NoCommonColumns
        );
    }

    #[test]
    fn detail_is_capped_but_counts_stay_exact_and_incomplete_inputs_flag() {
        let cols = vec![col("v", "int")];
        let left: Vec<Vec<CellValue>> = (0..100).map(|n| vec![i(n)]).collect();
        let right: Vec<Vec<CellValue>> = (50..200).map(|n| vec![i(n)]).collect();
        let options = DiffOptions {
            max_reported_rows: 5,
            ..DiffOptions::default()
        };
        let truncated = OutputView::new(&cols, &right, Completeness::Truncated { cap: 150 });
        let d = diff_outputs(&view(&cols, &left), &truncated, &options).unwrap();
        assert_eq!(
            (d.summary.added, d.summary.removed, d.summary.unchanged),
            (100, 50, 50)
        );
        assert_eq!(d.added.len(), 5);
        assert!(d.detail_truncated);
        assert!(!d.is_conclusive());
        assert_eq!(
            d,
            diff_outputs(&view(&cols, &left), &truncated, &options).unwrap()
        );
    }

    #[test]
    fn nulls_nan_and_unicode_compare_sensibly() {
        let cols = vec![col("k", "int"), col("v", "text")];
        let left = vec![
            vec![i(1), CellValue::Null],
            vec![i(2), f(f64::NAN)],
            vec![i(3), s("Straße")],
        ];
        let right = vec![
            vec![i(1), s("")],
            vec![i(2), f(f64::NAN)],
            vec![i(3), s("STRASSE")],
        ];
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &keyed(&["k"])).unwrap();
        assert_eq!(d.summary.changed, 2, "NULL ≠ '' and case differs");
        assert_eq!(change_text(&d.changed[0].changes[0].left), "NULL");
        let ci = DiffOptions {
            case_insensitive: true,
            ..keyed(&["k"])
        };
        let d = diff_outputs(&view(&cols, &left), &view(&cols, &right), &ci).unwrap();
        assert_eq!(d.summary.changed, 2, "lowercasing does not fold ß to ss");
        assert!(numbers_match(f64::NAN, f64::NAN, None));
        assert!(!numbers_match(
            f64::INFINITY,
            1e308,
            Some(Tolerance {
                absolute: 1e308,
                relative: 1.0
            })
        ));
    }
}
