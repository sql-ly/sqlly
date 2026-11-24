//! `--cells` and `--mode` → a planner [`RunRequest`].
//!
//! Grammar (cells by name or id):
//! - `orders`: the cell and whichever of its inputs are not current.
//! - `orders+`: the cell and everything downstream of it.
//! - `..orders`: every cell above it (not including it).
//! - `orders..`: the cell and every cell below it.
//! - `a,b,c`: those cells, each with the inputs it needs.
//!
//! Without `--cells`, `--mode all` runs everything and `--mode stale` only
//! what is not current against the last recorded run.

use sqlly_services::notebook::graph::Node;
use sqlly_services::notebook::{Graph, NotebookDoc, RunRequest};

use crate::agent::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum RunModeArg {
    /// Run every selected cell.
    #[default]
    All,
    /// Reuse outputs from the last run whose fingerprints still match.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Cell(String),
    Downstream(String),
    Above(String),
    Below(String),
}

fn parse_token(raw: &str) -> Result<Token, CliError> {
    let text = raw.trim();
    let bad = || CliError::usage(format!("invalid --cells entry '{raw}'"));
    if let Some(rest) = text.strip_prefix("..") {
        return non_empty(rest).map(Token::Above).ok_or_else(bad);
    }
    if let Some(rest) = text.strip_suffix("..") {
        return non_empty(rest).map(Token::Below).ok_or_else(bad);
    }
    if let Some(rest) = text.strip_suffix('+') {
        return non_empty(rest).map(Token::Downstream).ok_or_else(bad);
    }
    non_empty(text).map(Token::Cell).ok_or_else(bad)
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.contains(['.', '+'])).then(|| text.to_string())
}

/// The id of the cell `token` names (name first, then id).
fn cell_id(doc: &NotebookDoc, token: &str) -> Result<String, CliError> {
    doc.resolve(token)
        .or_else(|| doc.cell(token))
        .map(|cell| cell.id.clone())
        .ok_or_else(|| CliError::usage(format!("no cell is named or has the id '{token}'")))
}

/// Build the request for `--cells` / `--mode`.
pub fn request(
    doc: &NotebookDoc,
    graph: &Graph,
    cells: Option<&str>,
    mode: RunModeArg,
) -> Result<RunRequest, CliError> {
    let Some(cells) = cells.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(match mode {
            RunModeArg::All => RunRequest::All,
            RunModeArg::Stale => RunRequest::Stale,
        });
    };
    let tokens = cells
        .split(',')
        .map(parse_token)
        .collect::<Result<Vec<_>, _>>()?;
    if let [token] = tokens.as_slice() {
        return Ok(match token {
            Token::Cell(name) => RunRequest::Cell {
                id: cell_id(doc, name)?,
                include_stale_upstream: true,
            },
            Token::Downstream(name) => RunRequest::Downstream(cell_id(doc, name)?),
            Token::Above(name) => RunRequest::Above(cell_id(doc, name)?),
            Token::Below(name) => RunRequest::Below(cell_id(doc, name)?),
        });
    }
    let mut wanted: Vec<Node> = Vec::new();
    for token in &tokens {
        let Token::Cell(name) = token else {
            return Err(CliError::usage(
                "`+`, `..name` and `name..` take a single cell; list plain cells to run several",
            ));
        };
        let id = cell_id(doc, name)?;
        wanted.extend(graph.upstream_closure(&Node::cell(id.as_str())));
        wanted.push(Node::cell(id));
    }
    let ids = graph
        .in_order(wanted)
        .into_iter()
        .filter_map(|node| node.as_cell().map(str::to_string))
        .collect();
    Ok(RunRequest::Selected(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;
    use sqlly_services::notebook::{CellKind, Dialects, SeededIds};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::empty("nb_1", "Test");
        let mut ids = SeededIds(1);
        for (name, sql) in [
            ("orders", "SELECT 1 AS id"),
            ("big", "SELECT * FROM {{orders}}"),
            ("other", "SELECT 2"),
        ] {
            let i = doc.add_cell(CellKind::Sql, None, &mut ids);
            doc.cells[i].name = Some(name.into());
            doc.cells[i].source = sql.into();
        }
        doc
    }

    fn graph(doc: &NotebookDoc) -> Graph {
        Graph::build(doc, &Dialects::single(DatabaseType::Sqlite))
    }

    #[test]
    fn no_selection_follows_the_mode() {
        let d = doc();
        let g = graph(&d);
        assert_eq!(
            request(&d, &g, None, RunModeArg::All).unwrap(),
            RunRequest::All
        );
        assert_eq!(
            request(&d, &g, Some("  "), RunModeArg::Stale).unwrap(),
            RunRequest::Stale
        );
    }

    #[test]
    fn single_tokens_map_to_planner_requests() {
        let d = doc();
        let g = graph(&d);
        let id = |name: &str| d.resolve(name).unwrap().id.clone();
        assert_eq!(
            request(&d, &g, Some("big"), RunModeArg::All).unwrap(),
            RunRequest::Cell {
                id: id("big"),
                include_stale_upstream: true
            }
        );
        assert_eq!(
            request(&d, &g, Some("orders+"), RunModeArg::All).unwrap(),
            RunRequest::Downstream(id("orders"))
        );
        assert_eq!(
            request(&d, &g, Some("..big"), RunModeArg::All).unwrap(),
            RunRequest::Above(id("big"))
        );
        assert_eq!(
            request(&d, &g, Some("big.."), RunModeArg::All).unwrap(),
            RunRequest::Below(id("big"))
        );
        // Ids work as well as names.
        assert_eq!(
            request(&d, &g, Some(&id("other")), RunModeArg::All).unwrap(),
            RunRequest::Cell {
                id: id("other"),
                include_stale_upstream: true
            }
        );
    }

    #[test]
    fn lists_pull_in_inputs_in_document_order() {
        let d = doc();
        let g = graph(&d);
        let id = |name: &str| d.resolve(name).unwrap().id.clone();
        assert_eq!(
            request(&d, &g, Some("other, big"), RunModeArg::All).unwrap(),
            RunRequest::Selected(vec![id("orders"), id("big"), id("other")])
        );
    }

    #[test]
    fn bad_selections_are_usage_errors() {
        let d = doc();
        let g = graph(&d);
        for bad in ["nope", "..", "+", "a..b", "orders+,big", "orders,"] {
            let err = request(&d, &g, Some(bad), RunModeArg::All).unwrap_err();
            assert_eq!(err.exit, crate::agent::ExitCode::Usage, "{bad}");
        }
    }
}
