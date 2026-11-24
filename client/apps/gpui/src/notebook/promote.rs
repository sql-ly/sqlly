//! Moving SQL between query tabs and notebooks: "Open as Notebook" turns a
//! query tab into a notebook (optionally one cell per statement), and
//! "Convert to SQL script" flattens a notebook back into one script. SQL text
//! is copied verbatim both ways.

use std::collections::BTreeSet;

use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::notebook::{
    CellKind, ConnectionRef, Dialects, ExpandOptions, Expansion, NotebookDoc, RandomIds, expand_sql,
};

/// A notebook whose SQL cells carry `sql`: one cell, or one per statement
/// (split on `;` and `GO` the way Run splits them) when `split` is set.
/// `connection` becomes the notebook's default connection. Every cell keeps
/// running the way the tab did: statements that change data get writes
/// allowed, the rest are read-only.
pub fn notebook_from_query(
    sql: &str,
    connection: Option<ConnectionRef>,
    split: bool,
    db: DatabaseType,
) -> NotebookDoc {
    let mut doc = super::files::new_document();
    doc.cells.clear();
    doc.connection_refs.extend(connection);
    let pieces: Vec<String> = if split {
        crate::editor::statements::statement_spans_for(sql, db.family())
            .into_iter()
            .map(|span| sql[span.start..span.end].trim().to_string())
            .filter(|piece| !piece.is_empty())
            .collect()
    } else {
        vec![sql.to_string()]
    };
    let pieces = if pieces.is_empty() {
        vec![String::new()]
    } else {
        pieces
    };
    for piece in pieces {
        let index = doc.add_cell(CellKind::Sql, None, &mut RandomIds);
        let writes = sqlly_services::notebook::migrate::writes_data(Some(db), &piece);
        let cell = &mut doc.cells[index];
        cell.execution_policy.read_only = !writes;
        cell.source = piece;
    }
    doc
}

/// The notebook as one SQL script: SQL cells in order (references expanded
/// into the CTEs they run as when the core can inline them), Markdown cells
/// as `--` comments, cells of other kinds left out.
pub fn script_from_notebook(doc: &NotebookDoc, db: DatabaseType) -> String {
    let dialects = Dialects::single(db);
    let materialized = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    for cell in &doc.cells {
        match cell.kind {
            CellKind::Sql => {
                if cell.source.trim().is_empty() {
                    continue;
                }
                let expanded = expand_sql(
                    doc,
                    &cell.id,
                    ExpandOptions {
                        dialects: &dialects,
                        materialized: &materialized,
                    },
                );
                let sql = match expanded {
                    Ok(Expansion::Sql(e)) => e.sql,
                    _ => cell.source.clone(),
                };
                let sql = sql.trim_end();
                if sql.ends_with(';') {
                    parts.push(sql.to_string());
                } else {
                    parts.push(format!("{sql};"));
                }
            }
            CellKind::Markdown => {
                if cell.source.trim().is_empty() {
                    continue;
                }
                let comment: Vec<String> = cell
                    .source
                    .lines()
                    .map(|line| {
                        if line.is_empty() {
                            "--".to_string()
                        } else {
                            format!("-- {line}")
                        }
                    })
                    .collect();
                parts.push(comment.join("\n"));
            }
            _ => {}
        }
    }
    let mut out = parts.join("\n\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_query_becomes_one_cell_verbatim() {
        let sql = "SELECT *\r\nFROM dbo.Orders -- keep me\nWHERE 1 = 1";
        let doc = notebook_from_query(sql, None, false, DatabaseType::SqlServer);
        assert_eq!(doc.cells.len(), 1);
        assert_eq!(doc.cells[0].source, sql);
        assert!(doc.cells[0].execution_policy.read_only);
    }

    #[test]
    fn split_on_semicolons_and_go_and_mark_writes() {
        let sql = "SELECT 1;\nUPDATE t SET a = 1;\nGO\nSELECT 2";
        let doc = notebook_from_query(
            sql,
            Some(ConnectionRef {
                id: "conn_1".into(),
                profile_id: Some("p".into()),
                label: "Dev".into(),
                database: Some("Northwind".into()),
                extra: Default::default(),
            }),
            true,
            DatabaseType::SqlServer,
        );
        let sources: Vec<&str> = doc.cells.iter().map(|c| c.source.as_str()).collect();
        assert_eq!(sources, ["SELECT 1;", "UPDATE t SET a = 1;", "SELECT 2"]);
        assert!(!doc.cells[1].execution_policy.read_only);
        assert!(doc.cells[0].execution_policy.read_only);
        assert_eq!(doc.connection_refs[0].profile_id.as_deref(), Some("p"));
    }

    #[test]
    fn empty_query_still_gets_a_cell() {
        let doc = notebook_from_query("", None, true, DatabaseType::Postgres);
        assert_eq!(doc.cells.len(), 1);
    }

    #[test]
    fn script_round_trips_sql_text_and_comments_markdown() {
        let mut doc = NotebookDoc::empty("nb", "t");
        for (kind, source) in [
            (CellKind::Markdown, "# Orders\n\nLate ones"),
            (CellKind::Sql, "SELECT OrderID FROM dbo.Orders"),
            (CellKind::Chart, ""),
        ] {
            let at = doc.add_cell(kind, None, &mut RandomIds);
            doc.cells[at].source = source.into();
        }
        let script = script_from_notebook(&doc, DatabaseType::SqlServer);
        assert_eq!(
            script,
            "-- # Orders\n--\n-- Late ones\n\nSELECT OrderID FROM dbo.Orders;\n"
        );
    }
}
