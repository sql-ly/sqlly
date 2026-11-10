//! App-side multi-column sort spec. The datatable owns a single primary key;
//! secondaries are applied as a stable pre-order of source rows.

use serde::{Deserialize, Serialize};
use sqlly_datatable::{CellValue, SortDirection, compare_cells};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
}

/// Ordered sort keys. `keys[0]` is the primary key (mirrors the grid's own
/// `GridState.sort`); `keys[1..]` are secondaries applied app-side as a
/// stable pre-order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SortSpec {
    pub keys: Vec<SortKey>,
}

impl SortSpec {
    pub fn primary(&self) -> Option<(usize, SortDirection)> {
        let key = self.keys.first()?;
        Some((
            key.column,
            if key.descending {
                SortDirection::Descending
            } else {
                SortDirection::Ascending
            },
        ))
    }

    pub fn secondaries(&self) -> &[SortKey] {
        self.keys.get(1..).unwrap_or(&[])
    }

    /// Absent → append asc; asc → desc; desc → remove.
    pub fn add_or_toggle(&mut self, column: usize) {
        if let Some(i) = self.keys.iter().position(|k| k.column == column) {
            if !self.keys[i].descending {
                self.keys[i].descending = true;
            } else {
                self.keys.remove(i);
            }
        } else {
            self.keys.push(SortKey {
                column,
                descending: false,
            });
        }
    }

    pub fn remove(&mut self, column: usize) {
        self.keys.retain(|k| k.column != column);
    }

    /// Called when the grid's own sort changes; keeps secondaries, drops a
    /// duplicate of the new primary. Clearing the grid sort clears the spec.
    pub fn set_primary(&mut self, primary: Option<(usize, SortDirection)>) {
        match primary {
            None => self.keys.clear(),
            Some((column, dir)) => {
                let key = SortKey {
                    column,
                    descending: matches!(dir, SortDirection::Descending),
                };
                if self.keys.is_empty() {
                    self.keys.push(key);
                    return;
                }
                self.keys[0] = key;
                let mut i = 1;
                while i < self.keys.len() {
                    if self.keys[i].column == column {
                        self.keys.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }

    /// Stable order of `indices` by `keys[1..]` (last key first, so the first
    /// secondary dominates).
    pub fn order_secondary(&self, rows: &[Vec<CellValue>], indices: &mut [usize]) {
        for key in self.secondaries().iter().rev() {
            indices.sort_by(|&a, &b| {
                let left = rows.get(a).and_then(|row| row.get(key.column));
                let right = rows.get(b).and_then(|row| row.get(key.column));
                let ord = match (left, right) {
                    (Some(l), Some(r)) => compare_cells(l, r),
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (Some(_), None) => std::cmp::Ordering::Greater,
                };
                if key.descending { ord.reverse() } else { ord }
            });
        }
    }

    pub fn chip_text(&self, names: &[String]) -> String {
        let parts: Vec<String> = self
            .keys
            .iter()
            .map(|key| {
                let name = names
                    .get(key.column)
                    .cloned()
                    .unwrap_or_else(|| format!("col{}", key.column));
                let arrow = if key.descending { "↓" } else { "↑" };
                format!("{name} {arrow}")
            })
            .collect();
        crate::localization::format(
            "results.grid_sort.sort_chip",
            &[("column_directions", &parts.join(", "))],
        )
    }

    pub fn is_valid_for(&self, column_count: usize) -> bool {
        self.keys.iter().all(|k| k.column < column_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(column: usize, descending: bool) -> SortKey {
        SortKey { column, descending }
    }

    #[test]
    fn add_toggle_remove_cycle() {
        let mut spec = SortSpec::default();
        spec.add_or_toggle(2);
        assert_eq!(spec.keys, vec![key(2, false)]);
        spec.add_or_toggle(2);
        assert_eq!(spec.keys, vec![key(2, true)]);
        spec.add_or_toggle(2);
        assert!(spec.keys.is_empty());
        spec.add_or_toggle(0);
        spec.add_or_toggle(1);
        spec.remove(0);
        assert_eq!(spec.keys, vec![key(1, false)]);
    }

    #[test]
    fn set_primary_drops_duplicate_secondary() {
        let mut spec = SortSpec {
            keys: vec![key(0, false), key(1, true), key(2, false)],
        };
        spec.set_primary(Some((1, SortDirection::Ascending)));
        assert_eq!(spec.keys, vec![key(1, false), key(2, false)]);
        spec.set_primary(None);
        assert!(spec.keys.is_empty());
    }

    #[test]
    fn order_secondary_is_stable_and_last_key_first() {
        let rows = vec![
            vec![CellValue::Text("a".into()), CellValue::Integer(1)],
            vec![CellValue::Text("a".into()), CellValue::Integer(2)],
            vec![CellValue::Text("b".into()), CellValue::Integer(1)],
            vec![CellValue::Text("b".into()), CellValue::Integer(2)],
        ];
        let spec = SortSpec {
            keys: vec![key(9, false), key(0, false), key(1, true)],
        };
        let mut indices: Vec<usize> = vec![0, 1, 2, 3];
        spec.order_secondary(&rows, &mut indices);
        assert_eq!(indices, vec![1, 0, 3, 2]);
    }

    #[test]
    fn chip_text_uses_names_and_arrows() {
        let spec = SortSpec {
            keys: vec![key(1, false), key(0, true)],
        };
        let names = vec!["id".into(), "name".into()];
        assert_eq!(spec.chip_text(&names), "Sort: name ↑, id ↓");
    }

    #[test]
    fn invalid_for_shorter_column_count() {
        let spec = SortSpec {
            keys: vec![key(0, false), key(2, true)],
        };
        assert!(spec.is_valid_for(3));
        assert!(!spec.is_valid_for(2));
        assert!(SortSpec::default().is_valid_for(0));
    }

    #[test]
    fn compare_matches_datatable() {
        let a = CellValue::Integer(2);
        let b = CellValue::Integer(10);
        let c = CellValue::None;
        let d = CellValue::Text("B".into());
        let e = CellValue::Text("a".into());
        assert_eq!(compare_cells(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(compare_cells(&c, &a), std::cmp::Ordering::Less);
        assert_eq!(compare_cells(&d, &e), std::cmp::Ordering::Less);
        let rows = vec![vec![a.clone()], vec![b.clone()], vec![c.clone()]];
        let spec = SortSpec {
            keys: vec![key(0, false), key(0, false)],
        };
        let mut indices = vec![2, 1, 0];
        spec.order_secondary(&rows, &mut indices);
        assert_eq!(indices, vec![2, 0, 1]);
    }
}
