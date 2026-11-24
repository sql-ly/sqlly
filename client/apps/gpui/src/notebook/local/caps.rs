//! Enforcing a notebook cell's `max_rows` and `max_download_bytes` on the
//! row stream: rows past the cap are dropped from the batch that crosses
//! it, and the run is then stopped and reported as cut short (spec 5.4: the
//! user decides whether to fetch everything).

use std::collections::{HashMap, HashSet};

use sqlly_core::CellValue;

use super::Truncation;
use super::run::RunCaps;

/// Counts rows and bytes per result set against a cell's caps.
#[derive(Debug, Clone, Default)]
pub struct RowCapper {
    caps: RunCaps,
    rows: HashMap<usize, u64>,
    bytes: u64,
    /// Result sets withheld into a spill file that passed the row cap.
    spilled_past: HashSet<usize>,
}

/// Bytes one value takes on the wire, roughly: text and binary lengths,
/// eight for anything else.
fn value_bytes(value: &CellValue) -> u64 {
    match value {
        CellValue::String(s) => s.len() as u64,
        CellValue::Bytes(b) => b.len() as u64,
        CellValue::Null => 1,
        _ => 8,
    }
}

impl RowCapper {
    pub fn new(caps: RunCaps) -> Option<Self> {
        (!caps.is_empty()).then(|| Self {
            caps,
            ..Self::default()
        })
    }

    /// Admit `rows` of result set `index`, dropping what passes a cap.
    /// Returns where the cap was hit, once it is.
    pub fn admit(&mut self, index: usize, rows: &mut Vec<Vec<CellValue>>) -> Option<Truncation> {
        let seen = self.rows.entry(index).or_insert(0);
        let mut keep = rows.len();
        let mut by_bytes = false;
        if let Some(max) = self.caps.max_rows {
            let room = max.saturating_sub(*seen) as usize;
            if rows.len() > room {
                keep = room;
            }
        }
        if let Some(max) = self.caps.max_bytes {
            let mut bytes = self.bytes;
            for (i, row) in rows.iter().take(keep).enumerate() {
                let next = bytes + row.iter().map(value_bytes).sum::<u64>();
                if next > max {
                    keep = i;
                    by_bytes = true;
                    break;
                }
                bytes = next;
            }
            self.bytes = bytes;
        }
        let hit = keep < rows.len();
        rows.truncate(keep);
        *seen += keep as u64;
        hit.then_some(Truncation {
            rows_kept: *seen,
            by_bytes,
        })
    }
}

impl RowCapper {
    /// A result set past the spill threshold reports only how many rows it
    /// has seen (its rows go to a spill file, not through [`Self::admit`]);
    /// the row cap still stops it. Returns where the cap was hit, once.
    pub fn observe(&mut self, index: usize, rows_seen: u64) -> Option<Truncation> {
        let max = self.caps.max_rows?;
        if rows_seen <= max || !self.spilled_past.insert(index) {
            return None;
        }
        Some(Truncation {
            rows_kept: max,
            by_bytes: false,
        })
    }

    /// A spilled row count held to the row cap, so the grid never pages
    /// past it.
    pub fn clamp(&self, rows: usize) -> usize {
        self.caps.max_rows.map_or(rows, |max| {
            rows.min(usize::try_from(max).unwrap_or(usize::MAX))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Vec<CellValue>> {
        (0..n)
            .map(|i| vec![CellValue::I64(i as i64), CellValue::String("abcd".into())])
            .collect()
    }

    #[test]
    fn a_cap_above_the_spill_threshold_stops_the_spilled_set() {
        let mut capper = RowCapper::new(RunCaps {
            max_rows: Some(120_000),
            max_bytes: None,
        })
        .unwrap();
        let mut forwarded = rows(50_000);
        assert_eq!(capper.admit(0, &mut forwarded), None);
        assert_eq!(capper.observe(0, 90_000), None);
        assert_eq!(
            capper.observe(0, 131_072),
            Some(Truncation {
                rows_kept: 120_000,
                by_bytes: false
            })
        );
        assert_eq!(capper.observe(0, 140_000), None, "reported once");
        assert_eq!(capper.clamp(131_072), 120_000);
        assert_eq!(capper.clamp(10), 10);
        let bytes_only = RowCapper::new(RunCaps {
            max_rows: None,
            max_bytes: Some(10),
        })
        .unwrap();
        assert_eq!(bytes_only.clamp(500_000), 500_000);
    }

    #[test]
    fn no_caps_means_no_capper() {
        assert!(RowCapper::new(RunCaps::default()).is_none());
    }

    #[test]
    fn the_row_cap_trims_the_crossing_batch() {
        let mut capper = RowCapper::new(RunCaps {
            max_rows: Some(250),
            max_bytes: None,
        })
        .unwrap();
        let mut a = rows(200);
        assert_eq!(capper.admit(0, &mut a), None);
        assert_eq!(a.len(), 200);
        let mut b = rows(200);
        assert_eq!(
            capper.admit(0, &mut b),
            Some(Truncation {
                rows_kept: 250,
                by_bytes: false
            })
        );
        assert_eq!(b.len(), 50);
        let mut other = rows(10);
        assert_eq!(capper.admit(1, &mut other), None, "each result set counts");
    }

    #[test]
    fn the_byte_cap_stops_at_the_last_whole_row() {
        let mut capper = RowCapper::new(RunCaps {
            max_rows: None,
            max_bytes: Some(12 * 10 + 5),
        })
        .unwrap();
        let mut batch = rows(100);
        let hit = capper.admit(0, &mut batch).unwrap();
        assert_eq!(batch.len(), 10);
        assert!(hit.by_bytes);
        assert_eq!(hit.rows_kept, 10);
    }
}
