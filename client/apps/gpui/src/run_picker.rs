//! Pure model for the Run Statements picker: list statements, tick a subset,
//! join them into one batch for the existing mutation-review gate.
//!
//! Not migrated to `gpui_component::list::List`: this module is UI-free, and
//! its one rendering consumer (`render_run_picker_dialog` in `crate::app`) is
//! a checkbox multi-select, not a pick-one list. Every row is an
//! independently toggleable `Checkbox` reached via `tab_stop` traversal, with
//! Select all / None controls and a confirm button that runs the checked
//! batch. The kit list models exactly one `selected_index` with Enter
//! confirming that row — it has no per-row checked state, and its
//! focus-owning `ListState` would capture arrow/enter handling that conflicts
//! with the dialog's checkbox tab order and Run button.

use sqlly_lang::sqlmodel::Dialect;

use crate::editor::statements::{StatementSpan, statement_spans_for};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Ddl,
    Other,
}

impl StatementKind {
    pub fn label(self) -> &'static str {
        match self {
            StatementKind::Select => "SELECT",
            StatementKind::Insert => "INSERT",
            StatementKind::Update => "UPDATE",
            StatementKind::Delete => "DELETE",
            StatementKind::Ddl => "DDL",
            StatementKind::Other => "SQL",
        }
    }

    pub fn is_write(self) -> bool {
        matches!(
            self,
            StatementKind::Insert
                | StatementKind::Update
                | StatementKind::Delete
                | StatementKind::Ddl
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub index: usize,
    pub span: StatementSpan,
    pub preview: String,
    pub kind: StatementKind,
    pub checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPickerModel {
    pub rows: Vec<PickerRow>,
    pub dialect: Dialect,
}

impl RunPickerModel {
    /// Rows for every span; `checked` defaults to true for the statement under
    /// `cursor` (if `Some`) else all.
    pub fn from_sql(sql: &str, dialect: Dialect, cursor: Option<usize>) -> Self {
        let spans = statement_spans_for(sql, dialect);
        let active = cursor.and_then(|off| {
            spans
                .iter()
                .position(|span| off >= span.start && off < span.end)
        });
        let check_all = active.is_none();
        let rows = spans
            .into_iter()
            .enumerate()
            .map(|(index, span)| {
                let text = &sql[span.start..span.end];
                PickerRow {
                    index,
                    span,
                    preview: preview_line(text),
                    kind: statement_kind(text),
                    checked: check_all || active == Some(index),
                }
            })
            .collect();
        Self { rows, dialect }
    }

    #[allow(dead_code)]
    pub fn toggle(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row.checked = !row.checked;
        }
    }

    pub fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(row) = self.rows.get_mut(index) {
            row.checked = checked;
        }
    }

    pub fn set_all(&mut self, checked: bool) {
        for row in &mut self.rows {
            row.checked = checked;
        }
    }

    pub fn selected_spans(&self) -> Vec<StatementSpan> {
        self.rows
            .iter()
            .filter(|row| row.checked)
            .map(|row| row.span)
            .collect()
    }

    /// Join checked statements in buffer order. Returns `None` when nothing is checked.
    pub fn batch_sql(&self, sql: &str) -> Option<String> {
        let selected: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row.checked)
            .map(|row| row.span)
            .collect();
        if selected.is_empty() {
            return None;
        }
        let mut out = String::new();
        for (i, span) in selected.iter().enumerate() {
            if i > 0 {
                let gap = &sql[selected[i - 1].end..span.start];
                let sep = if self.dialect == Dialect::SqlServer && gap_has_go_line(gap) {
                    "\nGO\n"
                } else {
                    "\n"
                };
                out.push_str(sep);
            }
            out.push_str(sql[span.start..span.end].trim_end());
        }
        Some(out)
    }

    pub fn first_selected_start(&self) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.checked)
            .map(|row| row.span.start)
    }

    pub fn summary(&self) -> String {
        let total = self.rows.len();
        let selected = self.rows.iter().filter(|row| row.checked).count();
        let mut writes = 0usize;
        let mut update = 0usize;
        for row in self.rows.iter().filter(|row| row.checked) {
            if row.kind.is_write() {
                writes += 1;
            }
            if row.kind == StatementKind::Update {
                update += 1;
            }
        }
        let selected = selected.to_string();
        let total = total.to_string();
        let update_count = update.to_string();
        let write_count = writes.to_string();
        let (key, arguments): (&str, Vec<(&str, &str)>) = if update > 0 {
            (
                "query.run_picker.summary_updates",
                vec![
                    ("selected", &selected),
                    ("total", &total),
                    ("update_count", &update_count),
                ],
            )
        } else if writes > 0 {
            (
                "query.run_picker.summary_writes",
                vec![
                    ("selected", &selected),
                    ("total", &total),
                    ("write_count", &write_count),
                ],
            )
        } else {
            (
                "query.run_picker.summary",
                vec![("selected", &selected), ("total", &total)],
            )
        };
        crate::localization::format(key, &arguments)
    }
}

/// Keyword-based kind for a single statement span.
pub fn statement_kind(sql: &str) -> StatementKind {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return StatementKind::Other;
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("WITH ")
        && (upper.contains(" SELECT ") || upper.contains("\nSELECT "))
        && sqlly_intel::query::safety::classify_select_only(trimmed).select_only
    {
        return StatementKind::Select;
    }
    let first = first_keyword(trimmed);
    match first.as_deref() {
        Some("SELECT") => {
            if sqlly_intel::query::safety::classify_select_only(trimmed).select_only {
                StatementKind::Select
            } else {
                StatementKind::Other
            }
        }
        Some("INSERT") => StatementKind::Insert,
        Some("UPDATE") => StatementKind::Update,
        Some("DELETE") => StatementKind::Delete,
        Some("MERGE") => StatementKind::Update,
        Some("CREATE") | Some("ALTER") | Some("DROP") | Some("TRUNCATE") => StatementKind::Ddl,
        _ => StatementKind::Other,
    }
}

fn preview_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= 80 {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(79).collect();
        format!("{truncated}…")
    }
}

fn first_keyword(sql: &str) -> Option<String> {
    let mut chars = sql.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '-' && chars.clone().nth(1) == Some('-') {
            while chars.next().is_some_and(|c| c != '\n') {}
            continue;
        }
        if c == '/' && chars.clone().nth(1) == Some('*') {
            chars.next();
            chars.next();
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if !word.is_empty() {
            return Some(word.to_ascii_uppercase());
        }
        chars.next();
    }
    None
}

fn gap_has_go_line(gap: &str) -> bool {
    gap.lines().any(|line| {
        let t = line.trim();
        t.len() >= 2
            && t[..2].eq_ignore_ascii_case("go")
            && t[2..]
                .chars()
                .all(|c| matches!(c, ' ' | '\t' | '-' | '/' | '*'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_flips_checked_state() {
        let sql = "SELECT 1;\nSELECT 2;";
        let mut model = RunPickerModel::from_sql(sql, Dialect::SqlServer, None);
        assert!(model.rows[0].checked);
        model.toggle(0);
        assert!(!model.rows[0].checked);
    }

    #[test]
    fn rows_preview_and_kind() {
        let sql = "SELECT 1;\nUPDATE t SET x = 1;";
        let model = RunPickerModel::from_sql(sql, Dialect::SqlServer, None);
        assert_eq!(model.rows.len(), 2);
        assert_eq!(model.rows[0].preview, "SELECT 1;");
        assert_eq!(model.rows[0].kind, StatementKind::Select);
        assert_eq!(model.rows[1].kind, StatementKind::Update);
    }

    #[test]
    fn default_check_is_cursor_statement() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let cursor = sql.find('3').unwrap();
        let model = RunPickerModel::from_sql(sql, Dialect::SqlServer, Some(cursor));
        assert!(!model.rows[0].checked);
        assert!(!model.rows[1].checked);
        assert!(model.rows[2].checked);
    }

    #[test]
    fn batch_sql_joins_selected_in_order() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let mut model = RunPickerModel::from_sql(sql, Dialect::Postgres, None);
        model.set_all(false);
        model.toggle(0);
        model.toggle(2);
        let batch = model.batch_sql(sql).unwrap();
        assert!(batch.contains("SELECT 1"));
        assert!(batch.contains("SELECT 3"));
        assert!(!batch.contains("SELECT 2"));
    }

    #[test]
    fn batch_sql_keeps_go_between_batches() {
        let sql = "SELECT 1\nGO\nSELECT 2";
        let model = RunPickerModel::from_sql(sql, Dialect::SqlServer, None);
        let batch = model.batch_sql(sql).unwrap();
        assert!(batch.contains("GO"));
    }

    #[test]
    fn batch_sql_none_when_nothing_checked() {
        let sql = "SELECT 1;";
        let mut model = RunPickerModel::from_sql(sql, Dialect::SqlServer, None);
        model.set_all(false);
        assert!(model.batch_sql(sql).is_none());
    }

    #[test]
    fn summary_counts_writes() {
        let sql = "SELECT 1;\nUPDATE t SET x = 1;\nDELETE FROM u;";
        let mut model = RunPickerModel::from_sql(sql, Dialect::SqlServer, None);
        model.set_all(true);
        let summary = model.summary();
        assert!(summary.contains("3 of 3"));
        assert!(summary.contains("UPDATE"));
    }

    #[test]
    fn pg_dollar_body_is_single_row() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;\nSELECT 1;";
        let model = RunPickerModel::from_sql(sql, Dialect::Postgres, None);
        assert_eq!(model.rows.len(), 2);
        assert!(model.rows[0].preview.contains("CREATE FUNCTION"));
    }
}
