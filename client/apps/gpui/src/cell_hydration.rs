use sqlly_services::{CellLimits, TruncatedCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydratePurpose {
    Viewer,
    Hex,
    Copy,
    Dock,
}

/// Human size: `512 B`, `1.5 KiB`, `2.4 MiB`, `1.1 GiB` (one decimal above bytes).
pub fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    let (key, value) = if bytes < KIB {
        ("results.cell_hydration.size_bytes", bytes.to_string())
    } else if bytes < MIB {
        (
            "results.cell_hydration.size_kib",
            format!("{:.1}", bytes as f64 / KIB as f64),
        )
    } else if bytes < GIB {
        (
            "results.cell_hydration.size_mib",
            format!("{:.1}", bytes as f64 / MIB as f64),
        )
    } else {
        (
            "results.cell_hydration.size_gib",
            format!("{:.1}", bytes as f64 / GIB as f64),
        )
    };
    crate::localization::format(key, &[("size", &value)])
}

/// Grid marker appended to a preview: `" … [2.4 MiB]"` (leading space, ellipsis, size).
pub fn truncated_marker(cell: &TruncatedCell) -> String {
    format!(" … [{}]", human_size(cell.total_len))
}

/// Why a cell cannot be hydrated, in user language. `None` means "go ahead".
pub fn hydration_blocker(
    cell: &TruncatedCell,
    key_columns: &[String],
    limits: &CellLimits,
) -> Option<String> {
    if cell.total_len > limits.hard_cap_bytes {
        return Some(crate::localization::format(
            "results.cell_hydration.viewer_limit_exceeded",
            &[
                ("value_size", &human_size(cell.total_len)),
                ("limit_size", &human_size(limits.hard_cap_bytes)),
            ],
        ));
    }
    if key_columns.is_empty() {
        return Some(crate::localization::text(
            "results.cell_hydration.missing_key_columns",
        ));
    }
    None
}

/// `CellValue::String` preview + sidecar → the text the viewer should show when hydration is impossible.
pub fn unavailable_banner(cell: &TruncatedCell, preview: &str, limits: &CellLimits) -> String {
    let truncation = crate::localization::format(
        "results.cell_hydration.unavailable_truncation",
        &[
            ("shown_size", &human_size(limits.preview_bytes as u64)),
            ("total_size", &human_size(cell.total_len)),
        ],
    );
    format!("{preview}\n\n{truncation}")
}

pub fn cell_byte_len(value: &sqlly_core::CellValue) -> u64 {
    match value {
        sqlly_core::CellValue::String(s) => s.len() as u64,
        sqlly_core::CellValue::Bytes(b) => b.len() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::TruncatedKind;

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(2_516_582), "2.4 MiB");
        assert_eq!(human_size(1_180_591_620), "1.1 GiB");
    }

    #[test]
    fn truncated_marker_shape() {
        let cell = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 2_516_582,
            kind: TruncatedKind::Text,
        };
        assert_eq!(truncated_marker(&cell), " … [2.4 MiB]");
    }

    #[test]
    fn hydration_blocker_cap_then_key() {
        let limits = CellLimits::default();
        let huge = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 33 * 1024 * 1024,
            kind: TruncatedKind::Text,
        };
        let msg = hydration_blocker(&huge, &[], &limits).unwrap();
        assert!(msg.contains("32.0 MiB"));
        let small = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 1024,
            kind: TruncatedKind::Text,
        };
        let msg = hydration_blocker(&small, &[], &limits).unwrap();
        assert!(msg.contains("key column"));
    }

    #[test]
    fn hydration_blocker_none_when_key_present() {
        let cell = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 1024,
            kind: TruncatedKind::Text,
        };
        assert!(hydration_blocker(&cell, &["id".into()], &CellLimits::default()).is_none());
    }

    #[test]
    fn unavailable_banner_mentions_both_sizes() {
        let cell = TruncatedCell {
            row: 0,
            column: 0,
            total_len: 2_516_582,
            kind: TruncatedKind::Text,
        };
        let banner = unavailable_banner(&cell, "preview", &CellLimits::default());
        assert!(banner.contains("8.0 KiB"));
        assert!(banner.contains("2.4 MiB"));
        assert!(banner.starts_with("preview"));
    }
}
