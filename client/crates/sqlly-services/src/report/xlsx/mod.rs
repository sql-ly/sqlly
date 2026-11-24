//! The hand-rolled, dependency-free `.xlsx` writer (moved here from the
//! desktop app so the CLI and notebook reports share it; the app keeps thin
//! re-export shims), plus the notebook report workbook built on it.

pub mod plain;
pub mod report;
pub mod workbook;
pub mod zip;

use sqlly_core::ResultColumn;

pub use plain::{excel_xlsx, excel_xlsx_with};
pub use workbook::{
    Cell, ConditionalBlock, ConditionalCondition, ConditionalRule, ConditionalStyle, RgbaColor,
    Sheet, SheetColumn, SheetImage, Workbook, defined_name, excel_width, strftime_to_excel, write,
};
pub use zip::{crc32, zip_archive};

/// The spreadsheet column label for a 1-based index (`1` → `A`, `27` → `AA`).
pub fn excel_column_label(index: usize) -> String {
    let mut remaining = std::cmp::max(1, index);
    let mut label = String::new();
    while remaining > 0 {
        let digit = (remaining - 1) % 26;
        label.insert(0, char::from(b'A' + digit as u8));
        remaining = (remaining - 1) / 26;
    }
    label
}

/// Excel defined-names (and a table's displayName) must begin with a letter
/// or underscore and contain only letters, digits, underscores, or periods.
pub fn sanitize_excel_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut result = String::new();
    for ch in trimmed.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        return "Results".to_string();
    }
    if let Some(first) = result.chars().next()
        && !first.is_alphabetic()
        && first != '_'
    {
        result.insert(0, '_');
    }
    result
}

/// Column names made unique (`id`, `id_2`, …) and never empty, for formats
/// that need distinct headers.
pub fn unique_column_names(columns: &[ResultColumn]) -> Vec<String> {
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    unique_names(&names)
}

pub fn unique_names(names: &[&str]) -> Vec<String> {
    let base_of = |name: &str| {
        if name.trim().is_empty() {
            "Column".to_string()
        } else {
            name.to_string()
        }
    };
    let reserved: std::collections::HashSet<String> = names.iter().map(|n| base_of(n)).collect();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    names
        .iter()
        .map(|n| {
            let base = base_of(n);
            if used.insert(base.clone()) {
                return base;
            }
            let mut n = 2;
            loop {
                let candidate = format!("{}_{}", base, n);
                if !reserved.contains(&candidate) && used.insert(candidate.clone()) {
                    return candidate;
                }
                n += 1;
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_labels() {
        assert_eq!(excel_column_label(1), "A");
        assert_eq!(excel_column_label(26), "Z");
        assert_eq!(excel_column_label(27), "AA");
        assert_eq!(excel_column_label(703), "AAA");
    }

    #[test]
    fn sanitize_excel_name_basic() {
        assert_eq!(sanitize_excel_name("Results"), "Results");
        assert_eq!(sanitize_excel_name("My Table!"), "My_Table_");
        assert_eq!(sanitize_excel_name(""), "Results");
        assert_eq!(sanitize_excel_name("123abc"), "_123abc");
    }

    #[test]
    fn unique_names_never_collide() {
        assert_eq!(
            unique_names(&["id", "id", "", "id_2"]),
            vec!["id", "id_3", "Column", "id_2"]
        );
    }
}
