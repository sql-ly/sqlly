//! Structural edits that need more than one cell: split at the cursor,
//! merge neighbours, move a block, and Markdown heading sections (what a
//! folded heading hides).

use std::ops::Range;

use crate::notebook::document::{Cell, CellKind, IdSource, NotebookDoc, new_cell_id};

fn splittable(kind: &CellKind) -> bool {
    matches!(kind, CellKind::Sql | CellKind::Markdown)
}

/// Split cell `id` at byte `at` of its source. The first half keeps the
/// cell (id, name, everything); the second half becomes a new cell of the
/// same kind, connection and policy right below it. Returns the new id, or
/// `None` when the cell cannot be split there (not SQL or Markdown, or one
/// half would be empty).
pub fn split_cell(
    doc: &mut NotebookDoc,
    id: &str,
    at: usize,
    ids: &mut dyn IdSource,
) -> Option<String> {
    let index = doc.cell_index(id)?;
    let cell = &doc.cells[index];
    if !splittable(&cell.kind) || at > cell.source.len() || !cell.source.is_char_boundary(at) {
        return None;
    }
    let head = cell.source[..at].trim_end().to_string();
    let tail = cell.source[at..].trim_start().to_string();
    if head.is_empty() || tail.is_empty() {
        return None;
    }
    let new_id = new_cell_id(ids, |c| doc.cell(c).is_some());
    let mut second = Cell::new(
        new_id.clone(),
        cell.kind.clone(),
        cell.execution_policy.clone(),
    );
    second.source = tail;
    second.connection_ref = cell.connection_ref.clone();
    second.session_group = cell.session_group.clone();
    doc.cells[index].source = head;
    doc.cells.insert(index + 1, second);
    Some(new_id)
}

/// Merge the cells `ids` (in document order, all SQL or all Markdown) into
/// the first of them; the rest are removed. SQL is joined as separate
/// statements, Markdown as paragraphs. The merged cell stays read-only if
/// any part was. Returns the removed ids.
pub fn merge_cells(doc: &mut NotebookDoc, ids: &[String]) -> Option<Vec<String>> {
    let mut indexes: Vec<usize> = ids.iter().filter_map(|id| doc.cell_index(id)).collect();
    indexes.sort_unstable();
    indexes.dedup();
    if indexes.len() < 2 {
        return None;
    }
    let kind = doc.cells[indexes[0]].kind.clone();
    if !splittable(&kind) || indexes.iter().any(|i| doc.cells[*i].kind != kind) {
        return None;
    }
    let mut source = String::new();
    let mut read_only = false;
    for i in &indexes {
        let cell = &doc.cells[*i];
        read_only |= cell.execution_policy.read_only;
        let text = cell.source.trim();
        if text.is_empty() {
            continue;
        }
        if !source.is_empty() {
            if kind == CellKind::Sql && !source.trim_end().ends_with(';') {
                source.push(';');
            }
            source.push_str("\n\n");
        }
        source.push_str(text);
    }
    let first = indexes[0];
    doc.cells[first].source = source;
    doc.cells[first].execution_policy.read_only = read_only;
    let mut removed = Vec::new();
    for i in indexes[1..].iter().rev() {
        removed.push(doc.cells.remove(*i).id);
    }
    removed.reverse();
    Some(removed)
}

/// Move the cells `ids` as one block so it starts at `to` (an index in the
/// document without the block). Keeps their relative order. Returns whether
/// anything moved.
pub fn move_block(doc: &mut NotebookDoc, ids: &[String], to: usize) -> bool {
    let mut indexes: Vec<usize> = ids.iter().filter_map(|id| doc.cell_index(id)).collect();
    indexes.sort_unstable();
    indexes.dedup();
    if indexes.is_empty() {
        return false;
    }
    let before: Vec<String> = doc.cells.iter().map(|c| c.id.clone()).collect();
    let mut block = Vec::with_capacity(indexes.len());
    for i in indexes.iter().rev() {
        block.push(doc.cells.remove(*i));
    }
    block.reverse();
    let to = to.min(doc.cells.len());
    for (n, cell) in block.into_iter().enumerate() {
        doc.cells.insert(to + n, cell);
    }
    doc.cells
        .iter()
        .map(|c| c.id.as_str())
        .ne(before.iter().map(String::as_str))
}

/// Where a block dropped before the cell at `target` (in the full document)
/// lands once the block is taken out.
pub fn drop_index(doc: &NotebookDoc, ids: &[String], target: usize) -> usize {
    let above = doc.cells[..target.min(doc.cells.len())]
        .iter()
        .filter(|c| ids.contains(&c.id))
        .count();
    target - above
}

/// The heading level (1–6) of a Markdown cell whose first non-empty line is
/// an ATX heading.
pub fn heading_level(cell: &Cell) -> Option<u8> {
    if cell.kind != CellKind::Markdown {
        return None;
    }
    let line = cell
        .source
        .lines()
        .find(|l| !l.trim().is_empty())?
        .trim_start();
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    (1..=6)
        .contains(&hashes)
        .then(|| &line[hashes..])
        .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        .map(|_| hashes as u8)
}

/// The cells a heading at `index` covers: everything after it up to the
/// next heading of the same or a higher level.
pub fn section(doc: &NotebookDoc, index: usize) -> Range<usize> {
    let Some(level) = doc.cells.get(index).and_then(heading_level) else {
        return index..index;
    };
    let end = doc.cells[index + 1..]
        .iter()
        .position(|c| heading_level(c).is_some_and(|l| l <= level))
        .map(|p| index + 1 + p)
        .unwrap_or(doc.cells.len());
    index + 1..end
}

/// Indexes hidden by the folded headings in `folded` (cell ids).
pub fn hidden_by_folds(doc: &NotebookDoc, folded: &dyn Fn(&Cell) -> bool) -> Vec<bool> {
    let mut hidden = vec![false; doc.cells.len()];
    let mut i = 0;
    while i < doc.cells.len() {
        if !hidden[i] && folded(&doc.cells[i]) && heading_level(&doc.cells[i]).is_some() {
            let range = section(doc, i);
            for h in &mut hidden[range.clone()] {
                *h = true;
            }
            i = range.end.max(i + 1);
        } else {
            i += 1;
        }
    }
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::SeededIds;

    fn doc(cells: &[(CellKind, &str)]) -> NotebookDoc {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(1));
        doc.cells.clear();
        for (n, (kind, source)) in cells.iter().enumerate() {
            let i = doc.add_cell(kind.clone(), None, &mut SeededIds(10 + n as u64));
            doc.cells[i].source = source.to_string();
        }
        doc
    }

    fn sources(doc: &NotebookDoc) -> Vec<&str> {
        doc.cells.iter().map(|c| c.source.as_str()).collect()
    }

    #[test]
    fn split_keeps_the_first_half_in_place() {
        let mut d = doc(&[(CellKind::Sql, "select 1;\nselect 2")]);
        let id = d.cells[0].id.clone();
        d.cells[0].name = Some("first".into());
        let new = split_cell(&mut d, &id, 9, &mut SeededIds(5)).unwrap();
        assert_eq!(sources(&d), vec!["select 1;", "select 2"]);
        assert_eq!(d.cells[0].id, id);
        assert_eq!(d.cells[1].id, new);
        assert!(d.cells[1].name.is_none());
        assert!(split_cell(&mut d, &id, 0, &mut SeededIds(6)).is_none());
    }

    #[test]
    fn merge_joins_statements_and_paragraphs() {
        let mut d = doc(&[
            (CellKind::Sql, "select 1"),
            (CellKind::Sql, "select 2;"),
            (CellKind::Markdown, "x"),
        ]);
        let ids: Vec<String> = d.cells.iter().map(|c| c.id.clone()).collect();
        d.cells[1].execution_policy.read_only = false;
        let removed = merge_cells(&mut d, &ids[..2]).unwrap();
        assert_eq!(removed, vec![ids[1].clone()]);
        assert_eq!(sources(&d), vec!["select 1;\n\nselect 2;", "x"]);
        assert!(d.cells[0].execution_policy.read_only);
        assert!(merge_cells(&mut d, &[ids[0].clone(), ids[2].clone()]).is_none());
    }

    #[test]
    fn move_block_keeps_order() {
        let mut d = doc(&[
            (CellKind::Sql, "a"),
            (CellKind::Sql, "b"),
            (CellKind::Sql, "c"),
            (CellKind::Sql, "d"),
        ]);
        let ids: Vec<String> = d.cells.iter().map(|c| c.id.clone()).collect();
        let block = vec![ids[0].clone(), ids[2].clone()];
        let to = drop_index(&d, &block, 4);
        assert!(move_block(&mut d, &block, to));
        assert_eq!(sources(&d), vec!["b", "d", "a", "c"]);
        assert!(!move_block(&mut d, &block, 2));
    }

    #[test]
    fn sections_end_at_the_next_heading_of_the_same_level() {
        let d = doc(&[
            (CellKind::Markdown, "# One"),
            (CellKind::Sql, "a"),
            (CellKind::Markdown, "## Sub"),
            (CellKind::Sql, "b"),
            (CellKind::Markdown, "# Two"),
            (CellKind::Sql, "c"),
            (CellKind::Markdown, "#hashtag"),
        ]);
        assert_eq!(section(&d, 0), 1..4);
        assert_eq!(section(&d, 2), 3..4);
        assert_eq!(section(&d, 4), 5..7);
        assert_eq!(heading_level(&d.cells[6]), None);
        let one = d.cells[0].id.clone();
        let hidden = hidden_by_folds(&d, &|c| c.id == one);
        assert_eq!(hidden, vec![false, true, true, true, false, false, false]);
    }
}
