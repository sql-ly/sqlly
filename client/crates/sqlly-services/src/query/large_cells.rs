//! Cut oversized text/binary cells down to previews before they reach any
//! consumer (grid, spill file, engine frame). The full value is re-read by
//! primary key on demand (see the GPUI `cell_hydration` module).

use sqlly_core::CellValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellLimits {
    /// Bytes kept per cell. Default 8 KiB.
    pub preview_bytes: usize,
    /// Values above this are never hydrated in full (viewer cap). Default 32 MiB.
    pub hard_cap_bytes: u64,
}

impl Default for CellLimits {
    fn default() -> Self {
        Self {
            preview_bytes: 8 * 1024,
            hard_cap_bytes: 32 * 1024 * 1024,
        }
    }
}

impl CellLimits {
    /// No preview cut: every cell is delivered whole. For consumers that
    /// write values out (exports, the CLI) rather than paint a grid. Memory
    /// stays bounded by the pipeline's spill: rows past the threshold or
    /// byte budget go to disk, not RAM.
    pub const fn full_values() -> Self {
        Self {
            preview_bytes: usize::MAX,
            hard_cap_bytes: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TruncatedKind {
    Text,
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TruncatedCell {
    /// Absolute row index within the result set (not within the batch).
    pub row: usize,
    pub column: usize,
    pub total_len: u64,
    pub kind: TruncatedKind,
}

impl TruncatedCell {
    pub fn exceeds_hard_cap(&self, limits: &CellLimits) -> bool {
        self.total_len > limits.hard_cap_bytes
    }
}

/// Truncate in place. `first_row` is the absolute index of `rows[0]`.
/// Returns one entry per cut cell; empty when nothing was cut (the common case — no allocation).
pub fn truncate_rows(
    rows: &mut [Vec<CellValue>],
    first_row: usize,
    limits: &CellLimits,
) -> Vec<TruncatedCell> {
    let preview = limits.preview_bytes;
    let mut truncated = Vec::new();
    for (row_offset, row) in rows.iter_mut().enumerate() {
        for (column, cell) in row.iter_mut().enumerate() {
            match cell {
                CellValue::String(s) if s.len() > preview => {
                    let total_len = s.len() as u64;
                    let mut end = preview;
                    while end > 0 && !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    s.truncate(end);
                    s.shrink_to_fit();
                    truncated.push(TruncatedCell {
                        row: first_row + row_offset,
                        column,
                        total_len,
                        kind: TruncatedKind::Text,
                    });
                }
                CellValue::Bytes(b) if b.len() > preview => {
                    let total_len = b.len() as u64;
                    b.truncate(preview);
                    b.shrink_to_fit();
                    truncated.push(TruncatedCell {
                        row: first_row + row_offset,
                        column,
                        total_len,
                        kind: TruncatedKind::Bytes,
                    });
                }
                _ => {}
            }
        }
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_cells_untouched_and_no_entries() {
        let mut rows = vec![vec![
            CellValue::String("hello".into()),
            CellValue::Bytes(vec![1, 2, 3]),
            CellValue::I64(42),
        ]];
        let out = truncate_rows(&mut rows, 0, &CellLimits::default());
        assert!(out.is_empty());
        assert_eq!(rows[0][0], CellValue::String("hello".into()));
        assert_eq!(rows[0][1], CellValue::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn long_text_cut_on_char_boundary() {
        let unit = "é";
        let s = unit.repeat(5000);
        let original_len = s.len();
        assert!(original_len > 8192);
        let mut rows = vec![vec![CellValue::String(s.clone())]];
        let out = truncate_rows(&mut rows, 0, &CellLimits::default());
        assert_eq!(out.len(), 1);
        let CellValue::String(preview) = &rows[0][0] else {
            panic!("expected string");
        };
        assert!(preview.len() <= 8192);
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
        assert_eq!(out[0].total_len, original_len as u64);
        assert_eq!(out[0].kind, TruncatedKind::Text);
    }

    #[test]
    fn full_values_limits_cut_nothing() {
        let text = "x".repeat(20 * 1024);
        let mut rows = vec![vec![
            CellValue::String(text.clone()),
            CellValue::Bytes(vec![7u8; 20 * 1024]),
        ]];
        let out = truncate_rows(&mut rows, 0, &CellLimits::full_values());
        assert!(out.is_empty());
        assert_eq!(rows[0][0], CellValue::String(text));
        assert_eq!(rows[0][1], CellValue::Bytes(vec![7u8; 20 * 1024]));
    }

    #[test]
    fn long_bytes_cut_and_reported() {
        let mut rows = vec![vec![CellValue::Bytes(vec![0u8; 20 * 1024])]];
        let out = truncate_rows(&mut rows, 5, &CellLimits::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 5);
        assert_eq!(out[0].column, 0);
        assert_eq!(out[0].total_len, (20 * 1024) as u64);
        assert_eq!(out[0].kind, TruncatedKind::Bytes);
        let CellValue::Bytes(b) = &rows[0][0] else {
            panic!("expected bytes");
        };
        assert_eq!(b.len(), 8192);
    }

    #[test]
    fn first_row_offsets_row_index() {
        let mut rows = vec![
            vec![CellValue::String("ok".into())],
            vec![CellValue::String("ok".into())],
            vec![CellValue::String("x".repeat(20 * 1024))],
        ];
        let out = truncate_rows(&mut rows, 1000, &CellLimits::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 1002);
    }

    #[test]
    fn hard_cap_flag() {
        let cell = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 33 * 1024 * 1024,
            kind: TruncatedKind::Text,
        };
        assert!(cell.exceeds_hard_cap(&CellLimits::default()));
        let small = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 1024,
            kind: TruncatedKind::Text,
        };
        assert!(!small.exceeds_hard_cap(&CellLimits::default()));
    }
}
