//! Bounded merge-join compare for keyed table rows and sync-script generation.

#![forbid(unsafe_code)]

pub mod script;

use std::cmp::Ordering;
use std::collections::VecDeque;

use sqlly_core::{CellValue, ResultColumn};

/// Rows scanned per side before the compare stops and reports truncation.
pub const MAX_SCANNED_ROWS_PER_SIDE: usize = 1_000_000;
/// Differences retained (only-left + only-right + changed) before the compare stops.
pub const MAX_DIFFERENCES: usize = 50_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ChangedPair {
    pub key: Vec<CellValue>,
    pub left: Vec<CellValue>,
    pub right: Vec<CellValue>,
    pub changed_columns: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataDiff {
    pub only_left: Vec<Vec<CellValue>>,
    pub only_right: Vec<Vec<CellValue>>,
    pub changed: Vec<ChangedPair>,
    pub unchanged: usize,
    pub scanned_left: usize,
    pub scanned_right: usize,
    pub truncated: Option<sqlly_core::DisplayMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareError {
    MissingColumn { side: Side, column: String },
    NullKey { side: Side, row: usize },
    DuplicateKey { side: Side, row: usize },
    OutOfOrder { side: Side, row: usize },
    NoKeyColumns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl CompareError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::MissingColumn { side, column } => {
                let (key, side_name) = match side {
                    Side::Left => ("services.data_compare.errors.missing_column_left", "left"),
                    Side::Right => ("services.data_compare.errors.missing_column_right", "right"),
                };
                DisplayMessage::new(
                    key,
                    format!("Column {column} is missing on the {side_name} side"),
                )
                .with_arg("column_name", column)
            }
            Self::NullKey { side, row } => keyed_row_error(side, *row, "null"),
            Self::DuplicateKey { side, row } => keyed_row_error(side, *row, "duplicate"),
            Self::OutOfOrder { side, row } => keyed_row_error(side, *row, "out_of_order"),
            Self::NoKeyColumns => DisplayMessage::new(
                "services.data_compare.errors.no_key_columns",
                "Select at least one key column before comparing",
            ),
        }
    }
}

fn keyed_row_error(side: &Side, row: usize, kind: &str) -> sqlly_core::DisplayMessage {
    let side_name = match side {
        Side::Left => "left",
        Side::Right => "right",
    };
    let key = match (side_name, kind) {
        ("left", "null") => "services.data_compare.errors.null_key_left",
        ("right", "null") => "services.data_compare.errors.null_key_right",
        ("left", "duplicate") => "services.data_compare.errors.duplicate_key_left",
        ("right", "duplicate") => "services.data_compare.errors.duplicate_key_right",
        ("left", "out_of_order") => "services.data_compare.errors.out_of_order_left",
        ("right", "out_of_order") => "services.data_compare.errors.out_of_order_right",
        _ => unreachable!(),
    };
    let problem = match kind {
        "null" => "has a NULL key",
        "duplicate" => "has a duplicate key",
        _ => "is out of key order",
    };
    sqlly_core::DisplayMessage::new(key, format!("Row {row} on the {side_name} side {problem}"))
        .with_arg("row_number", row.to_string())
}

/// Map every LEFT column name to its RIGHT index (case-insensitive, first match).
pub fn align_columns(
    left: &[ResultColumn],
    right: &[ResultColumn],
) -> Result<Vec<usize>, CompareError> {
    let mut map = Vec::with_capacity(left.len());
    for column in left {
        let index = right
            .iter()
            .position(|candidate| candidate.name.eq_ignore_ascii_case(&column.name))
            .ok_or_else(|| CompareError::MissingColumn {
                side: Side::Right,
                column: column.name.clone(),
            })?;
        map.push(index);
    }
    Ok(map)
}

/// Resolve key column NAMES to LEFT indexes (case-insensitive).
pub fn key_indexes(
    left: &[ResultColumn],
    key_names: &[String],
) -> Result<Vec<usize>, CompareError> {
    if key_names.is_empty() {
        return Err(CompareError::NoKeyColumns);
    }
    let mut indexes = Vec::with_capacity(key_names.len());
    for name in key_names {
        let index = left
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| CompareError::MissingColumn {
                side: Side::Left,
                column: name.clone(),
            })?;
        indexes.push(index);
    }
    Ok(indexes)
}

fn type_rank(cell: &CellValue) -> u8 {
    match cell {
        CellValue::Null => 0,
        CellValue::Bool(_) => 1,
        CellValue::I64(_) | CellValue::F64(_) => 2,
        CellValue::String(_) => 3,
        CellValue::Bytes(_) => 4,
    }
}

fn numeric_key(cell: &CellValue) -> Option<(f64, u8)> {
    match cell {
        CellValue::I64(n) => Some((*n as f64, 0)),
        CellValue::F64(f) => Some((*f, 1)),
        _ => None,
    }
}

/// Total order used for merge-join and for verifying server `ORDER BY`.
pub fn compare_cells(a: &CellValue, b: &CellValue) -> Ordering {
    let rank_a = type_rank(a);
    let rank_b = type_rank(b);
    match rank_a.cmp(&rank_b) {
        Ordering::Equal => {}
        other => return other,
    }
    match (a, b) {
        (CellValue::Null, CellValue::Null) => Ordering::Equal,
        (CellValue::Bool(x), CellValue::Bool(y)) => x.cmp(y),
        (CellValue::I64(x), CellValue::I64(y)) => x.cmp(y),
        (CellValue::F64(x), CellValue::F64(y)) => {
            if x.is_nan() && y.is_nan() {
                Ordering::Equal
            } else {
                x.partial_cmp(y).unwrap_or(Ordering::Equal)
            }
        }
        (CellValue::I64(_), CellValue::F64(_)) | (CellValue::F64(_), CellValue::I64(_)) => {
            let (fx, tx) = numeric_key(a).unwrap();
            let (fy, ty) = numeric_key(b).unwrap();
            fx.partial_cmp(&fy)
                .unwrap_or(Ordering::Equal)
                .then_with(|| tx.cmp(&ty))
        }
        (CellValue::String(x), CellValue::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (CellValue::Bytes(x), CellValue::Bytes(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Lexicographic key comparison.
pub fn compare_keys(a: &[CellValue], b: &[CellValue]) -> Ordering {
    for (left, right) in a.iter().zip(b.iter()) {
        let ordering = compare_cells(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

fn cell_equal(a: &CellValue, b: &CellValue) -> bool {
    match (a, b) {
        (CellValue::Null, CellValue::Null) => true,
        (CellValue::Bool(x), CellValue::Bool(y)) => x == y,
        (CellValue::I64(x), CellValue::I64(y)) => x == y,
        (CellValue::F64(x), CellValue::F64(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (CellValue::String(x), CellValue::String(y)) => x == y,
        (CellValue::Bytes(x), CellValue::Bytes(y)) => x == y,
        _ => false,
    }
}

fn extract_key(row: &[CellValue], key_indexes: &[usize]) -> Vec<CellValue> {
    key_indexes
        .iter()
        .map(|index| row.get(*index).cloned().unwrap_or(CellValue::Null))
        .collect()
}

pub struct MergeJoiner {
    key_indexes: Vec<usize>,
    right_to_left: Vec<usize>,
    left_pending: VecDeque<Vec<CellValue>>,
    right_pending: VecDeque<Vec<CellValue>>,
    left_done: bool,
    right_done: bool,
    last_left_key: Option<Vec<CellValue>>,
    last_right_key: Option<Vec<CellValue>>,
    diff: DataDiff,
    max_scanned_per_side: usize,
    max_differences: usize,
}

impl MergeJoiner {
    pub fn new(
        left: &[ResultColumn],
        right: &[ResultColumn],
        key_names: &[String],
    ) -> Result<Self, CompareError> {
        Self::with_caps(
            left,
            right,
            key_names,
            MAX_SCANNED_ROWS_PER_SIDE,
            MAX_DIFFERENCES,
        )
    }

    pub(crate) fn with_caps(
        left: &[ResultColumn],
        right: &[ResultColumn],
        key_names: &[String],
        max_scanned_per_side: usize,
        max_differences: usize,
    ) -> Result<Self, CompareError> {
        let key_indexes = key_indexes(left, key_names)?;
        let right_to_left = align_columns(left, right)?;
        Ok(Self {
            key_indexes,
            right_to_left,
            left_pending: VecDeque::new(),
            right_pending: VecDeque::new(),
            left_done: false,
            right_done: false,
            last_left_key: None,
            last_right_key: None,
            diff: DataDiff::default(),
            max_scanned_per_side,
            max_differences,
        })
    }

    pub fn push_left(&mut self, rows: Vec<Vec<CellValue>>) -> Result<(), CompareError> {
        if self.diff.truncated.is_some() {
            return Ok(());
        }
        for row in rows {
            self.diff.scanned_left += 1;
            if self.diff.scanned_left > self.max_scanned_per_side {
                self.set_truncated(Side::Left);
                return Ok(());
            }
            let key = extract_key(&row, &self.key_indexes);
            if key.iter().any(|cell| matches!(cell, CellValue::Null)) {
                return Err(CompareError::NullKey {
                    side: Side::Left,
                    row: self.diff.scanned_left,
                });
            }
            if let Some(last) = &self.last_left_key {
                match compare_keys(&key, last) {
                    Ordering::Less => {
                        return Err(CompareError::OutOfOrder {
                            side: Side::Left,
                            row: self.diff.scanned_left,
                        });
                    }
                    Ordering::Equal => {
                        return Err(CompareError::DuplicateKey {
                            side: Side::Left,
                            row: self.diff.scanned_left,
                        });
                    }
                    Ordering::Greater => {}
                }
            }
            self.last_left_key = Some(key);
            self.left_pending.push_back(row);
        }
        Ok(())
    }

    pub fn push_right(&mut self, rows: Vec<Vec<CellValue>>) -> Result<(), CompareError> {
        if self.diff.truncated.is_some() {
            return Ok(());
        }
        for row in rows {
            self.diff.scanned_right += 1;
            if self.diff.scanned_right > self.max_scanned_per_side {
                self.set_truncated(Side::Right);
                return Ok(());
            }
            let key = extract_key(&row, &self.key_indexes);
            if key.iter().any(|cell| matches!(cell, CellValue::Null)) {
                return Err(CompareError::NullKey {
                    side: Side::Right,
                    row: self.diff.scanned_right,
                });
            }
            if let Some(last) = &self.last_right_key {
                match compare_keys(&key, last) {
                    Ordering::Less => {
                        return Err(CompareError::OutOfOrder {
                            side: Side::Right,
                            row: self.diff.scanned_right,
                        });
                    }
                    Ordering::Equal => {
                        return Err(CompareError::DuplicateKey {
                            side: Side::Right,
                            row: self.diff.scanned_right,
                        });
                    }
                    Ordering::Greater => {}
                }
            }
            self.last_right_key = Some(key);
            self.right_pending.push_back(row);
        }
        Ok(())
    }

    pub fn left_finished(&mut self) {
        self.left_done = true;
    }

    pub fn right_finished(&mut self) {
        self.right_done = true;
    }

    pub fn drain(&mut self) -> bool {
        if self.diff.truncated.is_some() {
            return false;
        }
        loop {
            if self.left_done && self.left_pending.is_empty() {
                while let Some(row) = self.right_pending.pop_front() {
                    if !self.record_only_right(row) {
                        return false;
                    }
                }
            }
            if self.right_done && self.right_pending.is_empty() {
                while let Some(row) = self.left_pending.pop_front() {
                    if !self.record_only_left(row) {
                        return false;
                    }
                }
            }
            match (self.left_pending.front(), self.right_pending.front()) {
                (None, None) => return !(self.left_done && self.right_done),
                (Some(_), None) if !self.right_done => return true,
                (None, Some(_)) if !self.left_done => return true,
                (Some(_), None) => continue,
                (None, Some(_)) => continue,
                (Some(_), Some(_)) => {}
            }

            let left_row = self.left_pending.front().cloned().unwrap();
            let right_row = self.right_pending.front().cloned().unwrap();
            let left_key = extract_key(&left_row, &self.key_indexes);
            let right_key = extract_key(&right_row, &self.key_indexes);
            match compare_keys(&left_key, &right_key) {
                Ordering::Less => {
                    self.left_pending.pop_front();
                    if !self.record_only_left(left_row) {
                        return false;
                    }
                }
                Ordering::Greater => {
                    self.right_pending.pop_front();
                    if !self.record_only_right(right_row) {
                        return false;
                    }
                }
                Ordering::Equal => {
                    self.left_pending.pop_front();
                    self.right_pending.pop_front();
                    let right_aligned = self.reorder_right(&right_row);
                    let mut changed_columns = Vec::new();
                    for (index, (left_cell, right_cell)) in
                        left_row.iter().zip(right_aligned.iter()).enumerate()
                    {
                        if self.key_indexes.contains(&index) {
                            continue;
                        }
                        if !cell_equal(left_cell, right_cell) {
                            changed_columns.push(index);
                        }
                    }
                    if changed_columns.is_empty() {
                        self.diff.unchanged += 1;
                    } else if !self.record_changed(left_row, right_aligned, changed_columns) {
                        return false;
                    }
                }
            }
        }
    }

    pub fn diff(&self) -> &DataDiff {
        &self.diff
    }

    pub fn into_diff(self) -> DataDiff {
        self.diff
    }

    fn reorder_right(&self, row: &[CellValue]) -> Vec<CellValue> {
        self.right_to_left
            .iter()
            .map(|index| row.get(*index).cloned().unwrap_or(CellValue::Null))
            .collect()
    }

    fn difference_count(&self) -> usize {
        self.diff.only_left.len() + self.diff.only_right.len() + self.diff.changed.len()
    }

    fn set_truncated(&mut self, side: Side) {
        let n = match side {
            Side::Left => self.diff.scanned_left,
            Side::Right => self.diff.scanned_right,
        };
        let label = match side {
            Side::Left => "left",
            Side::Right => "right",
        };
        let key = match side {
            Side::Left => "services.data_compare.truncated.rows_left",
            Side::Right => "services.data_compare.truncated.rows_right",
        };
        self.diff.truncated = Some(
            sqlly_core::DisplayMessage::new(
                key,
                format!("Stopped after {n} rows on the {label} side — narrow the comparison with a query or a WHERE clause."),
            )
            .with_arg("row_count", n.to_string()),
        );
    }

    fn at_difference_cap(&mut self) -> bool {
        if self.difference_count() >= self.max_differences {
            self.diff.truncated = Some(
                sqlly_core::DisplayMessage::new(
                    "services.data_compare.truncated.differences",
                    format!("Stopped after {} differences — narrow the comparison with a query or a WHERE clause.", self.max_differences),
                )
                .with_arg("difference_count", self.max_differences.to_string()),
            );
            true
        } else {
            false
        }
    }

    fn record_only_left(&mut self, row: Vec<CellValue>) -> bool {
        if self.at_difference_cap() {
            return false;
        }
        self.diff.only_left.push(row);
        !self.at_difference_cap()
    }

    fn record_only_right(&mut self, row: Vec<CellValue>) -> bool {
        if self.at_difference_cap() {
            return false;
        }
        self.diff.only_right.push(self.reorder_right(&row));
        !self.at_difference_cap()
    }

    fn record_changed(
        &mut self,
        left: Vec<CellValue>,
        right: Vec<CellValue>,
        changed_columns: Vec<usize>,
    ) -> bool {
        if self.at_difference_cap() {
            return false;
        }
        let key = extract_key(&left, &self.key_indexes);
        self.diff.changed.push(ChangedPair {
            key,
            left,
            right,
            changed_columns,
        });
        !self.at_difference_cap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: if name == "id" || name == "active" {
                "int".into()
            } else {
                "text".into()
            },
            nullable: true,
        }
    }

    fn row2(id: i64, name: &str) -> Vec<CellValue> {
        vec![CellValue::I64(id), CellValue::String(name.into())]
    }

    fn row_name_id(name: &str, id: i64) -> Vec<CellValue> {
        vec![CellValue::String(name.into()), CellValue::I64(id)]
    }

    fn finish(mut joiner: MergeJoiner) -> DataDiff {
        while joiner.drain() {}
        joiner.into_diff()
    }

    #[test]
    fn identical_sides() {
        let left = vec![col("id"), col("name")];
        let right = left.clone();
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&left, &right, &keys).unwrap();
        joiner.push_left(vec![row2(1, "a")]).unwrap();
        joiner.push_right(vec![row2(1, "a")]).unwrap();
        joiner.left_finished();
        joiner.right_finished();
        let diff = finish(joiner);
        assert_eq!(diff.unchanged, 1);
        assert!(diff.only_left.is_empty());
        assert!(diff.only_right.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn row_missing_on_right() {
        let left = vec![col("id"), col("name")];
        let right = left.clone();
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&left, &right, &keys).unwrap();
        joiner.push_left(vec![row2(1, "only")]).unwrap();
        joiner.left_finished();
        joiner.right_finished();
        let diff = finish(joiner);
        assert_eq!(diff.only_left.len(), 1);
    }

    #[test]
    fn row_missing_on_left_reorders_columns() {
        let left = vec![col("id"), col("name")];
        let right = vec![col("name"), col("id")];
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&left, &right, &keys).unwrap();
        joiner.push_right(vec![row_name_id("only", 2)]).unwrap();
        joiner.left_finished();
        joiner.right_finished();
        let diff = finish(joiner);
        assert_eq!(
            diff.only_right,
            vec![vec![CellValue::I64(2), CellValue::String("only".into())]]
        );
    }

    #[test]
    fn changed_non_key_value() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        joiner.push_left(vec![row2(1, "before")]).unwrap();
        joiner.push_right(vec![row2(1, "after")]).unwrap();
        joiner.left_finished();
        joiner.right_finished();
        let diff = finish(joiner);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].changed_columns, vec![1]);
    }

    #[test]
    fn null_key_rejected() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        let err = joiner
            .push_left(vec![vec![CellValue::Null, CellValue::String("x".into())]])
            .unwrap_err();
        assert_eq!(
            err,
            CompareError::NullKey {
                side: Side::Left,
                row: 1
            }
        );
    }

    #[test]
    fn duplicate_key_rejected() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        joiner.push_left(vec![row2(1, "a")]).unwrap();
        let err = joiner.push_left(vec![row2(1, "b")]).unwrap_err();
        assert_eq!(
            err,
            CompareError::DuplicateKey {
                side: Side::Left,
                row: 2
            }
        );
    }

    #[test]
    fn composite_key_sorts_lexicographically() {
        let columns = vec![col("id"), col("name"), col("extra")];
        let keys = vec!["id".to_string(), "name".to_string()];
        let mut joiner = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        joiner
            .push_left(vec![
                vec![
                    CellValue::I64(1),
                    CellValue::String("a".into()),
                    CellValue::String("same".into()),
                ],
                vec![
                    CellValue::I64(1),
                    CellValue::String("b".into()),
                    CellValue::String("left".into()),
                ],
            ])
            .unwrap();
        joiner
            .push_right(vec![
                vec![
                    CellValue::I64(1),
                    CellValue::String("a".into()),
                    CellValue::String("same".into()),
                ],
                vec![
                    CellValue::I64(1),
                    CellValue::String("b".into()),
                    CellValue::String("right".into()),
                ],
            ])
            .unwrap();
        joiner.left_finished();
        joiner.right_finished();
        let diff = finish(joiner);
        assert_eq!(diff.unchanged, 1);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].changed_columns, vec![2]);
    }

    #[test]
    fn out_of_order_push_rejected() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let mut joiner = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        joiner.push_left(vec![row2(3, "a")]).unwrap();
        let err = joiner.push_left(vec![row2(1, "b")]).unwrap_err();
        assert_eq!(
            err,
            CompareError::OutOfOrder {
                side: Side::Left,
                row: 2
            }
        );
    }

    #[test]
    fn missing_column_on_right() {
        let left = vec![col("id"), col("name")];
        let right = vec![col("id")];
        let keys = vec!["id".to_string()];
        assert!(matches!(
            MergeJoiner::new(&left, &right, &keys),
            Err(CompareError::MissingColumn { .. })
        ));
    }

    #[test]
    fn interleaved_batches_match_one_shot() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let mut chunked = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        chunked.push_left(vec![row2(1, "a")]).unwrap();
        chunked.push_right(vec![row2(1, "a")]).unwrap();
        chunked.push_left(vec![row2(2, "b")]).unwrap();
        chunked.push_right(vec![row2(3, "c")]).unwrap();
        chunked.left_finished();
        chunked.right_finished();
        let chunked_diff = finish(chunked);

        let mut one_shot = MergeJoiner::new(&columns, &columns, &keys).unwrap();
        one_shot
            .push_left(vec![row2(1, "a"), row2(2, "b")])
            .unwrap();
        one_shot
            .push_right(vec![row2(1, "a"), row2(3, "c")])
            .unwrap();
        one_shot.left_finished();
        one_shot.right_finished();
        let one_shot_diff = finish(one_shot);
        assert_eq!(chunked_diff, one_shot_diff);
    }

    #[test]
    fn difference_cap_stops_compare() {
        let columns = vec![col("id"), col("name")];
        let keys = vec!["id".to_string()];
        let cap = 3;
        let mut joiner = MergeJoiner::with_caps(&columns, &columns, &keys, 1_000, cap).unwrap();
        for id in 1..=cap + 1 {
            joiner.push_left(vec![row2(id as i64, "x")]).unwrap();
        }
        joiner.left_finished();
        joiner.right_finished();
        assert!(!joiner.drain());
        let diff = joiner.into_diff();
        assert!(diff.truncated.is_some());
        assert_eq!(diff.only_left.len(), cap);
    }

    #[test]
    fn compare_cells_ordering_table() {
        assert_eq!(
            compare_cells(&CellValue::Null, &CellValue::Bool(false)),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::Bool(false), &CellValue::Bool(true)),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::Bool(true), &CellValue::I64(1)),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::I64(2), &CellValue::F64(2.5)),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::F64(2.0), &CellValue::String("a".into())),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::String("a".into()), &CellValue::Bytes(vec![1])),
            Ordering::Less
        );
        assert_eq!(
            compare_cells(&CellValue::I64(2), &CellValue::F64(2.0)),
            Ordering::Less
        );
    }
}
