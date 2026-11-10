//! Pure Object Browser view-model: inventory rows, filtering, grid shaping,
//! and batch script assembly. No GPUI — the pane renders what this module
//! produces and never executes the generated SQL.

use std::collections::HashMap;

use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::table_actions::{
    ColumnDescriptor, QualifiedTable, ScriptObjectKind, create_table_ddl_for, drop_object_sql_for,
    truncate_sql_for,
};
use crate::tool_dialog::{ToolQueryOutcome, ToolQuerySet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryRow {
    pub kind: String,
    pub schema: String,
    pub name: String,
    pub row_estimate: Option<i64>,
    pub size_kb: Option<i64>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub comment: Option<String>,
}

impl InventoryRow {
    pub fn qualified_name(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }

    fn qualified_table(&self) -> QualifiedTable {
        let mut table = QualifiedTable::new(self.name.clone());
        if !self.schema.is_empty() {
            table = table.with_schema(self.schema.clone());
        }
        table
    }
}

pub fn rows_from_outcome(outcome: &ToolQueryOutcome) -> Vec<InventoryRow> {
    let mut rows = Vec::new();
    for set in &outcome.sets {
        for row in &set.rows {
            rows.push(InventoryRow {
                kind: cell_text(row.first()),
                schema: cell_text(row.get(1)),
                name: cell_text(row.get(2)),
                row_estimate: cell_i64(row.get(3)),
                size_kb: cell_i64(row.get(4)),
                created: cell_opt_text(row.get(5)),
                modified: cell_opt_text(row.get(6)),
                comment: cell_opt_text(row.get(7)),
            });
        }
    }
    rows
}

pub fn filter_rows<'a>(
    rows: &'a [InventoryRow],
    needle: &str,
    kinds: &[String],
) -> Vec<&'a InventoryRow> {
    let needle = needle.trim().to_lowercase();
    let kind_filter: Vec<String> = kinds
        .iter()
        .map(|kind| kind.trim().to_ascii_uppercase())
        .filter(|kind| !kind.is_empty())
        .collect();
    rows.iter()
        .filter(|row| {
            if !needle.is_empty() {
                let haystack = row.qualified_name().to_lowercase();
                if !haystack.contains(&needle) {
                    return false;
                }
            }
            if kind_filter.is_empty() {
                return true;
            }
            kind_filter
                .iter()
                .any(|kind| row.kind.eq_ignore_ascii_case(kind))
        })
        .collect()
}

pub fn grid_set(rows: &[&InventoryRow]) -> ToolQuerySet {
    let columns = vec![
        col(
            &crate::localization::text("explorer.object_browser.column_kind"),
            "nvarchar",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_schema"),
            "nvarchar",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_name"),
            "nvarchar",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_rows"),
            "int",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_size"),
            "int",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_created"),
            "nvarchar",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_modified"),
            "nvarchar",
        ),
        col(
            &crate::localization::text("explorer.object_browser.column_comment"),
            "nvarchar",
        ),
    ];
    let rows = rows
        .iter()
        .map(|row| {
            vec![
                CellValue::String(row.kind.clone()),
                CellValue::String(row.schema.clone()),
                CellValue::String(row.name.clone()),
                int_cell(row.row_estimate),
                int_cell(row.size_kb),
                opt_string_cell(&row.created),
                opt_string_cell(&row.modified),
                opt_string_cell(&row.comment),
            ]
        })
        .collect();
    ToolQuerySet { columns, rows }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchAction {
    Drop,
    Truncate,
    ScriptCreate,
    ScriptDrop,
    CopyNames,
}

impl BatchAction {
    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Drop => "explorer.object_browser.batch_drop",
            Self::Truncate => "explorer.object_browser.batch_truncate",
            Self::ScriptCreate => "explorer.object_browser.batch_script_create",
            Self::ScriptDrop => "explorer.object_browser.batch_script_drop",
            Self::CopyNames => "explorer.object_browser.batch_copy_names",
        })
    }
}

pub struct BatchScript {
    pub sql: String,
    pub skipped: Vec<String>,
}

pub fn batch_script(
    db: DatabaseType,
    database: &str,
    action: BatchAction,
    rows: &[&InventoryRow],
    columns_by_object: &HashMap<(String, String), Vec<ColumnDescriptor>>,
) -> BatchScript {
    if action == BatchAction::CopyNames {
        return BatchScript {
            sql: copy_names_text(rows, db),
            skipped: Vec::new(),
        };
    }

    let ordered: Vec<&InventoryRow> = match action {
        BatchAction::Drop | BatchAction::ScriptDrop => {
            let mut ordered = rows.to_vec();
            ordered.sort_by_key(|row| drop_rank(&row.kind));
            ordered
        }
        _ => rows.to_vec(),
    };

    let mut statements = Vec::new();
    let mut skipped = Vec::new();
    for row in ordered {
        match script_for_row(db, action, row, columns_by_object) {
            Ok(sql) => statements.push(sql),
            Err(reason) => skipped.push(reason),
        }
    }

    let header = format!(
        "-- Object Browser: {} on {} objects in [{}]\n-- Review carefully before running.\n\n",
        action.label(),
        rows.len(),
        database
    );
    BatchScript {
        sql: format!("{header}{}", statements.join("\n\n")),
        skipped,
    }
}

pub fn copy_names_text(rows: &[&InventoryRow], db: DatabaseType) -> String {
    rows.iter()
        .map(|row| row.qualified_table().multipart_identifier_for(db))
        .collect::<Vec<_>>()
        .join("\n")
}

fn script_for_row(
    db: DatabaseType,
    action: BatchAction,
    row: &InventoryRow,
    columns_by_object: &HashMap<(String, String), Vec<ColumnDescriptor>>,
) -> Result<String, String> {
    let qt = row.qualified_table();
    match action {
        BatchAction::Drop | BatchAction::ScriptDrop => {
            let kind = script_kind(&row.kind).ok_or_else(|| {
                crate::localization::format(
                    "explorer.object_browser.drop_unsupported",
                    &[("object", &row.qualified_name()), ("kind", &row.kind)],
                )
            })?;
            Ok(drop_object_sql_for(db, kind, &qt))
        }
        BatchAction::Truncate => {
            if !row.kind.eq_ignore_ascii_case("TABLE") {
                return Err(crate::localization::format(
                    "explorer.object_browser.truncate_not_table",
                    &[("object", &row.qualified_name()), ("kind", &row.kind)],
                ));
            }
            Ok(truncate_sql_for(db, &qt))
        }
        BatchAction::ScriptCreate => {
            if row.kind.eq_ignore_ascii_case("TABLE")
                && let Some(columns) =
                    columns_by_object.get(&(row.schema.clone(), row.name.clone()))
            {
                return Ok(create_table_ddl_for(db, &qt, columns));
            }
            Ok(format!(
                "-- CREATE script for {} {}: open Properties or the DDL viewer",
                row.kind,
                row.qualified_name()
            ))
        }
        BatchAction::CopyNames => Ok(qt.multipart_identifier_for(db)),
    }
}

fn script_kind(kind: &str) -> Option<ScriptObjectKind> {
    match kind.to_ascii_uppercase().as_str() {
        "TABLE" => Some(ScriptObjectKind::Table),
        "VIEW" => Some(ScriptObjectKind::View),
        "PROCEDURE" => Some(ScriptObjectKind::Procedure),
        "FUNCTION" => Some(ScriptObjectKind::Function),
        _ => None,
    }
}

fn drop_rank(kind: &str) -> u8 {
    match kind.to_ascii_uppercase().as_str() {
        "VIEW" | "MATERIALIZED VIEW" | "PROCEDURE" | "FUNCTION" => 0,
        "TABLE" => 1,
        _ => 2,
    }
}

fn col(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable: true,
    }
}

fn int_cell(value: Option<i64>) -> CellValue {
    match value {
        Some(n) => CellValue::I64(n),
        None => CellValue::Null,
    }
}

fn opt_string_cell(value: &Option<String>) -> CellValue {
    match value {
        Some(text) if !text.is_empty() => CellValue::String(text.clone()),
        _ => CellValue::Null,
    }
}

fn cell_text(value: Option<&CellValue>) -> String {
    match value {
        Some(CellValue::String(s)) => s.clone(),
        Some(CellValue::Null) | None => String::new(),
        Some(CellValue::I64(n)) => n.to_string(),
        Some(CellValue::F64(n)) => n.to_string(),
        Some(CellValue::Bool(b)) => b.to_string(),
        Some(other) => format!("{other:?}"),
    }
}

fn cell_opt_text(value: Option<&CellValue>) -> Option<String> {
    let text = cell_text(value);
    if text.is_empty() { None } else { Some(text) }
}

fn cell_i64(value: Option<&CellValue>) -> Option<i64> {
    match value {
        Some(CellValue::I64(n)) => Some(*n),
        Some(CellValue::F64(n)) => Some(*n as i64),
        Some(CellValue::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, schema: &str, name: &str) -> InventoryRow {
        InventoryRow {
            kind: kind.into(),
            schema: schema.into(),
            name: name.into(),
            row_estimate: Some(10),
            size_kb: Some(32),
            created: None,
            modified: None,
            comment: None,
        }
    }

    #[test]
    fn filter_matches_schema_qualified_name_case_insensitively() {
        let rows = vec![
            row("TABLE", "dbo", "Orders"),
            row("VIEW", "sales", "OpenOrders"),
        ];
        let hits = filter_rows(&rows, "DBO.ord", &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Orders");
        assert!(
            filter_rows(&rows, "open", &[])
                .iter()
                .any(|r| r.name == "OpenOrders")
        );
    }

    #[test]
    fn filter_by_kind_subset() {
        let rows = vec![
            row("TABLE", "dbo", "Orders"),
            row("VIEW", "dbo", "vOrders"),
            row("PROCEDURE", "dbo", "usp_Orders"),
        ];
        let hits = filter_rows(&rows, "", &["view".into(), "PROCEDURE".into()]);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|r| r.kind != "TABLE"));
    }

    #[test]
    fn grid_set_uses_int_cells_for_numbers() {
        let rows = [row("TABLE", "dbo", "Orders")];
        let refs: Vec<&InventoryRow> = rows.iter().collect();
        let set = grid_set(&refs);
        assert_eq!(set.columns[3].name, "Rows (est.)");
        assert_eq!(set.columns[3].data_type, "int");
        assert_eq!(set.columns[4].name, "Size (KB)");
        assert_eq!(set.rows[0][3], CellValue::I64(10));
        assert_eq!(set.rows[0][4], CellValue::I64(32));
    }

    #[test]
    fn drop_batch_lists_views_before_tables() {
        let table = row("TABLE", "dbo", "Orders");
        let view = row("VIEW", "dbo", "vOrders");
        let selected = [&table, &view];
        let script = batch_script(
            DatabaseType::SqlServer,
            "Sales",
            BatchAction::Drop,
            &selected,
            &HashMap::new(),
        );
        let view_at = script.sql.find("DROP VIEW").expect("view drop");
        let table_at = script.sql.find("DROP TABLE").expect("table drop");
        assert!(view_at < table_at, "{}", script.sql);
        assert!(
            script
                .sql
                .contains("-- Object Browser: Drop on 2 objects in [Sales]")
        );
        assert!(script.sql.contains("IF EXISTS"));
        assert!(!script.sql.contains("\nGO"));
        assert!(script.skipped.is_empty());
    }

    #[test]
    fn truncate_batch_skips_views_with_reason() {
        let table = row("TABLE", "dbo", "Orders");
        let view = row("VIEW", "dbo", "vOrders");
        let selected = [&table, &view];
        let script = batch_script(
            DatabaseType::SqlServer,
            "Sales",
            BatchAction::Truncate,
            &selected,
            &HashMap::new(),
        );
        assert!(script.sql.contains("TRUNCATE TABLE"));
        assert!(
            !script
                .sql
                .to_ascii_uppercase()
                .contains("TRUNCATE TABLE [DBO].[VORDERS]")
        );
        assert_eq!(script.skipped.len(), 1);
        assert!(script.skipped[0].contains("vOrders"));
        assert!(script.skipped[0].contains("VIEW"));
    }

    #[test]
    fn script_create_uses_columns_when_present() {
        let table = row("TABLE", "dbo", "Orders");
        let view = row("VIEW", "dbo", "vOrders");
        let mut columns = HashMap::new();
        columns.insert(
            ("dbo".into(), "Orders".into()),
            vec![ColumnDescriptor {
                name: "Id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        );
        let selected = [&table, &view];
        let script = batch_script(
            DatabaseType::SqlServer,
            "Sales",
            BatchAction::ScriptCreate,
            &selected,
            &columns,
        );
        assert!(script.sql.contains("CREATE TABLE [dbo].[Orders]"));
        assert!(script.sql.contains("[Id]"));
        assert!(
            script.sql.contains(
                "-- CREATE script for VIEW dbo.vOrders: open Properties or the DDL viewer"
            )
        );
        assert!(script.skipped.is_empty());
    }

    #[test]
    fn copy_names_are_engine_quoted() {
        let table = row("TABLE", "dbo", "Orders");
        let selected = [&table];
        let tds = copy_names_text(&selected, DatabaseType::SqlServer);
        assert_eq!(tds, "[dbo].[Orders]");
        let pg = copy_names_text(&selected, DatabaseType::Postgres);
        assert_eq!(pg, "\"dbo\".\"Orders\"");
    }
}
