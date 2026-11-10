use crate::mcp::policy::MAX_ROWS_HARD_CAP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSpec {
    pub max_rows: usize,
    pub cell_char_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Windowed {
    pub text: String,
    pub rows_shown: usize,
    pub truncated_rows: bool,
    pub truncated_cells: usize,
}

pub fn clamp_limit(requested: Option<usize>, policy_max: usize) -> usize {
    let policy_max = policy_max.min(MAX_ROWS_HARD_CAP);
    let limit = requested.unwrap_or(policy_max);
    limit.clamp(1, policy_max.min(MAX_ROWS_HARD_CAP))
}

fn truncate_cell(cell: &str, limit: usize) -> (String, bool) {
    let char_count = cell.chars().count();
    if char_count <= limit {
        return (cell.to_string(), false);
    }
    let truncated: String = cell.chars().take(limit).collect();
    (format!("{truncated}…"), true)
}

pub fn render_table(
    columns: &[String],
    rows: &[Vec<String>],
    more_available: bool,
    spec: WindowSpec,
) -> Windowed {
    let rows = &rows[..rows.len().min(spec.max_rows)];
    let mut truncated_cells = 0usize;
    if columns.is_empty() {
        return Windowed {
            text: "(no columns)".into(),
            rows_shown: 0,
            truncated_rows: more_available,
            truncated_cells: 0,
        };
    }
    let mut lines = vec![columns.join(" | ")];
    for row in rows {
        let mut cells = Vec::with_capacity(columns.len());
        for (index, _column) in columns.iter().enumerate() {
            let raw = row.get(index).map(String::as_str).unwrap_or("NULL");
            let display = if raw.is_empty() { "NULL" } else { raw };
            let (cell, truncated) = truncate_cell(display, spec.cell_char_limit);
            if truncated {
                truncated_cells += 1;
            }
            cells.push(cell);
        }
        while cells.len() < columns.len() {
            cells.push(String::new());
        }
        lines.push(cells.join(" | "));
    }
    if rows.is_empty() {
        lines.push("(0 rows)".into());
    }
    if more_available {
        lines.push("(more rows available)".into());
    }
    Windowed {
        text: lines.join("\n"),
        rows_shown: rows.len(),
        truncated_rows: more_available,
        truncated_cells,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults() {
        assert_eq!(clamp_limit(None, 100), 100);
        assert_eq!(clamp_limit(Some(500), 100), 100);
        assert_eq!(clamp_limit(Some(0), 100), 1);
        assert_eq!(clamp_limit(Some(20), 50), 20);
    }

    #[test]
    fn multibyte_cell_truncation() {
        let cell = "你好世界你好世界";
        let (out, truncated) = truncate_cell(cell, 4);
        assert!(truncated);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn more_available_trailer() {
        let windowed = render_table(
            &["a".into()],
            &[vec!["1".into()]],
            true,
            WindowSpec {
                max_rows: 10,
                cell_char_limit: 200,
            },
        );
        assert!(windowed.text.contains("(more rows available)"));
        assert!(windowed.truncated_rows);
    }

    #[test]
    fn empty_rows_header() {
        let windowed = render_table(
            &["a".into()],
            &[],
            false,
            WindowSpec {
                max_rows: 10,
                cell_char_limit: 200,
            },
        );
        assert!(windowed.text.contains("(0 rows)"));
    }

    #[test]
    fn counts_truncated_cells() {
        let long = "x".repeat(5_000);
        let windowed = render_table(
            &["c".into()],
            &[vec![long]],
            false,
            WindowSpec {
                max_rows: 10,
                cell_char_limit: 200,
            },
        );
        assert_eq!(windowed.truncated_cells, 1);
        assert!(windowed.text.contains('…'));
    }
}
