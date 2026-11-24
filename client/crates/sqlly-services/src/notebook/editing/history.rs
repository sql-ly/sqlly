//! Structural undo and redo across a notebook: every change to cells, their
//! order, names, specs, connections, parameters or the title is one step,
//! captured as before/after snapshots of the document's authored content.
//!
//! Typing inside a cell editor is not a step (the editor keeps its own
//! undo): [`History::note_typed`] folds it into the baseline, and undoing a
//! step keeps the text typed since in every cell that step did not touch.
//! Snapshots share unchanged cells through `Arc`, so a long history of a
//! large notebook holds only the cells that changed.

use std::sync::Arc;

use crate::notebook::document::{
    Cell, ConnectionRef, DataRetention, NotebookDoc, NotebookSettings,
};
use crate::notebook::params::ParameterDef;

/// Steps kept; the oldest is dropped past this.
pub const DEFAULT_LIMIT: usize = 200;
/// Consecutive edits of the same field within this window are one step.
pub const COALESCE_MS: i64 = 1500;

#[derive(Debug, Clone, PartialEq)]
struct Meta {
    title: String,
    connection_refs: Vec<ConnectionRef>,
    parameters: Vec<ParameterDef>,
    settings: NotebookSettings,
    data_retention: DataRetention,
}

impl Meta {
    fn of(doc: &NotebookDoc) -> Self {
        Self {
            title: doc.title.clone(),
            connection_refs: doc.connection_refs.clone(),
            parameters: doc.parameters.clone(),
            settings: doc.settings.clone(),
            data_retention: doc.data_retention.clone(),
        }
    }
}

/// The authored content of a notebook at one moment. Machine markers in the
/// document's top-level `extra` (trust origin and the like) are not part of
/// it, so undo never moves them.
#[derive(Debug, Clone)]
pub struct Snapshot {
    meta: Arc<Meta>,
    cells: Vec<Arc<Cell>>,
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        (Arc::ptr_eq(&self.meta, &other.meta) || self.meta == other.meta)
            && self.cells.len() == other.cells.len()
            && self
                .cells
                .iter()
                .zip(&other.cells)
                .all(|(a, b)| Arc::ptr_eq(a, b) || a == b)
    }
}

impl Snapshot {
    pub fn of(doc: &NotebookDoc) -> Self {
        Self {
            meta: Arc::new(Meta::of(doc)),
            cells: doc.cells.iter().cloned().map(Arc::new).collect(),
        }
    }

    /// Capture `doc`, sharing every part that equals `previous`.
    pub fn capture(doc: &NotebookDoc, previous: &Snapshot) -> Self {
        let meta = Meta::of(doc);
        let meta = if *previous.meta == meta {
            previous.meta.clone()
        } else {
            Arc::new(meta)
        };
        let cells = doc
            .cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let same = previous
                    .cells
                    .get(i)
                    .filter(|p| p.id == cell.id)
                    .or_else(|| previous.cells.iter().find(|p| p.id == cell.id));
                match same {
                    Some(p) if **p == *cell => p.clone(),
                    _ => Arc::new(cell.clone()),
                }
            })
            .collect();
        Self { meta, cells }
    }

    fn cell(&self, id: &str) -> Option<&Arc<Cell>> {
        self.cells.iter().find(|c| c.id == id)
    }

    pub fn cell_ids(&self) -> Vec<String> {
        self.cells.iter().map(|c| c.id.clone()).collect()
    }

    /// Write this snapshot's content into `doc`.
    pub fn apply_to(&self, doc: &mut NotebookDoc) {
        doc.title = self.meta.title.clone();
        doc.connection_refs = self.meta.connection_refs.clone();
        doc.parameters = self.meta.parameters.clone();
        doc.settings = self.meta.settings.clone();
        doc.data_retention = self.meta.data_retention.clone();
        doc.cells = self.cells.iter().map(|c| (**c).clone()).collect();
    }

    fn shape_from(&self, before: &Snapshot) -> Shape {
        let same_order = self.cells.len() == before.cells.len()
            && self
                .cells
                .iter()
                .zip(&before.cells)
                .all(|(a, b)| a.id == b.id);
        if !same_order {
            return Shape::Structure;
        }
        let changed: Vec<&Arc<Cell>> = self
            .cells
            .iter()
            .zip(&before.cells)
            .filter(|(a, b)| !Arc::ptr_eq(a, b) && a != b)
            .map(|(a, _)| a)
            .collect();
        let meta_same = Arc::ptr_eq(&self.meta, &before.meta) || self.meta == before.meta;
        match (changed.as_slice(), meta_same) {
            ([cell], true) => Shape::Cell(cell.id.clone()),
            ([], false) => {
                let mut title_only = (*before.meta).clone();
                title_only.title = self.meta.title.clone();
                if title_only == *self.meta {
                    Shape::Title
                } else {
                    Shape::Notebook
                }
            }
            _ => Shape::Structure,
        }
    }
}

/// What one step changed, used to fold a burst of edits into one step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Structure,
    Cell(String),
    Title,
    Notebook,
}

#[derive(Debug, Clone)]
struct Step {
    label: &'static str,
    before: Snapshot,
    after: Snapshot,
    shape: Shape,
    at_ms: i64,
    /// Explicitly labelled steps never absorb later edits.
    sealed: bool,
}

/// What an undo or redo asks the host to apply.
#[derive(Debug, Clone)]
pub struct Restore {
    pub snapshot: Snapshot,
    /// Translation key naming the step (`notebook.editing.step.*`).
    pub label: &'static str,
}

/// Undo and redo stacks over snapshots, plus the baseline the next change
/// is measured against.
#[derive(Debug, Clone)]
pub struct History {
    baseline: Snapshot,
    undo: Vec<Step>,
    redo: Vec<Step>,
    limit: usize,
    next_label: Option<&'static str>,
}

/// Label of a step nothing named.
pub const GENERIC_LABEL: &str = "notebook.editing.step.edit";

impl History {
    pub fn new(doc: &NotebookDoc) -> Self {
        Self::with_limit(doc, DEFAULT_LIMIT)
    }

    pub fn with_limit(doc: &NotebookDoc, limit: usize) -> Self {
        Self {
            baseline: Snapshot::of(doc),
            undo: Vec::new(),
            redo: Vec::new(),
            limit: limit.max(1),
            next_label: None,
        }
    }

    /// Forget every step (the document was reloaded from disk).
    pub fn reset(&mut self, doc: &NotebookDoc) {
        self.baseline = Snapshot::of(doc);
        self.undo.clear();
        self.redo.clear();
        self.next_label = None;
    }

    /// Name the next step; it also stops it from merging with the previous.
    pub fn label_next(&mut self, label: &'static str) {
        self.next_label = Some(label);
    }

    /// Text typed in a cell's editor: part of the baseline, not a step.
    pub fn note_typed(&mut self, cell_id: &str, source: &str) {
        if let Some(cell) = self.baseline.cells.iter_mut().find(|c| c.id == cell_id)
            && cell.source != source
        {
            Arc::make_mut(cell).source = source.to_string();
        }
    }

    /// Accept `doc` as the baseline without a step (view state such as a
    /// folded section or a bookmark).
    pub fn note_untracked(&mut self, doc: &NotebookDoc) {
        self.baseline = Snapshot::capture(doc, &self.baseline);
    }

    /// Record the change from the baseline to `doc` as a step, merging it
    /// into the previous step when both edit the same single field within
    /// [`COALESCE_MS`]. Returns whether anything changed.
    pub fn observe(&mut self, doc: &NotebookDoc, now_ms: i64) -> bool {
        let label = self.next_label.take();
        let after = Snapshot::capture(doc, &self.baseline);
        if after == self.baseline {
            return false;
        }
        let shape = after.shape_from(&self.baseline);
        self.redo.clear();
        if label.is_none()
            && shape != Shape::Structure
            && let Some(top) = self.undo.last_mut()
            && !top.sealed
            && top.shape == shape
            && now_ms - top.at_ms <= COALESCE_MS
        {
            top.after = after.clone();
            top.at_ms = now_ms;
            self.baseline = after;
            return true;
        }
        self.undo.push(Step {
            label: label.unwrap_or(GENERIC_LABEL),
            before: std::mem::replace(&mut self.baseline, after.clone()),
            after,
            shape,
            at_ms: now_ms,
            sealed: label.is_some(),
        });
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&'static str> {
        self.undo.last().map(|s| s.label)
    }

    pub fn redo_label(&self) -> Option<&'static str> {
        self.redo.last().map(|s| s.label)
    }

    pub fn len(&self) -> usize {
        self.undo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.undo.is_empty()
    }

    /// Undo the latest step against the current `doc` (call
    /// [`Self::observe`] first so pending changes are a step of their own).
    pub fn undo(&mut self, doc: &NotebookDoc) -> Option<Restore> {
        let step = self.undo.pop()?;
        let current = Snapshot::capture(doc, &self.baseline);
        let target = carry_typing(&step.before, &step, &current);
        self.redo.push(Step {
            before: target.clone(),
            after: current,
            sealed: true,
            ..step.clone()
        });
        self.baseline = target.clone();
        Some(Restore {
            snapshot: target,
            label: step.label,
        })
    }

    pub fn redo(&mut self, doc: &NotebookDoc) -> Option<Restore> {
        let step = self.redo.pop()?;
        let current = Snapshot::capture(doc, &self.baseline);
        let target = carry_typing(&step.after, &step, &current);
        self.undo.push(Step {
            before: current,
            after: target.clone(),
            sealed: true,
            ..step.clone()
        });
        self.baseline = target.clone();
        Some(Restore {
            snapshot: target,
            label: step.label,
        })
    }
}

/// `target`, with the current text of every cell whose text `step` did not
/// change (what the user typed since stays).
fn carry_typing(target: &Snapshot, step: &Step, current: &Snapshot) -> Snapshot {
    let cells = target
        .cells
        .iter()
        .map(|cell| {
            let untouched = match (step.before.cell(&cell.id), step.after.cell(&cell.id)) {
                (Some(b), Some(a)) => b.source == a.source,
                _ => false,
            };
            match current.cell(&cell.id) {
                Some(now) if untouched && now.source != cell.source => {
                    let mut kept = (**cell).clone();
                    kept.source = now.source.clone();
                    Arc::new(kept)
                }
                _ => cell.clone(),
            }
        })
        .collect();
    Snapshot {
        meta: target.meta.clone(),
        cells,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CellKind, SeededIds};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(1));
        doc.cells[0].source = "select 1".into();
        doc.add_cell(CellKind::Markdown, None, &mut SeededIds(2));
        doc.cells[1].source = "# Notes".into();
        doc
    }

    fn sources(doc: &NotebookDoc) -> Vec<String> {
        doc.cells.iter().map(|c| c.source.clone()).collect()
    }

    #[test]
    fn insert_undo_redo_round_trips() {
        let mut d = doc();
        let mut h = History::new(&d);
        d.add_cell(CellKind::Sql, Some(1), &mut SeededIds(3));
        assert!(h.observe(&d, 0));
        assert_eq!(d.cells.len(), 3);
        let r = h.undo(&d).unwrap();
        r.snapshot.apply_to(&mut d);
        assert_eq!(d.cells.len(), 2);
        let r = h.redo(&d).unwrap();
        r.snapshot.apply_to(&mut d);
        assert_eq!(d.cells.len(), 3);
        assert!(!h.can_redo());
    }

    #[test]
    fn typing_is_not_a_step_and_survives_undo() {
        let mut d = doc();
        let mut h = History::new(&d);
        let id = d.cells[0].id.clone();
        d.add_cell(CellKind::Sql, None, &mut SeededIds(3));
        h.observe(&d, 0);
        d.cells[0].source = "select 2".into();
        h.note_typed(&id, "select 2");
        assert!(!h.observe(&d, 10));
        h.undo(&d).unwrap().snapshot.apply_to(&mut d);
        assert_eq!(d.cells.len(), 2);
        assert_eq!(d.cells[0].source, "select 2");
    }

    #[test]
    fn undoing_a_replace_restores_the_text_it_changed() {
        let mut d = doc();
        let mut h = History::new(&d);
        d.cells[0].source = "select 9".into();
        d.cells[1].source = "# Nine".into();
        h.label_next("notebook.editing.step.replace");
        h.observe(&d, 0);
        let r = h.undo(&d).unwrap();
        assert_eq!(r.label, "notebook.editing.step.replace");
        r.snapshot.apply_to(&mut d);
        assert_eq!(sources(&d), vec!["select 1", "# Notes"]);
    }

    #[test]
    fn bursts_on_one_field_coalesce_but_structure_does_not() {
        let mut d = doc();
        let mut h = History::new(&d);
        for (n, title) in ["a", "ab", "abc"].iter().enumerate() {
            d.title = title.to_string();
            h.observe(&d, n as i64 * 100);
        }
        assert_eq!(h.len(), 1);
        d.title = "abcd".into();
        h.observe(&d, 10_000);
        assert_eq!(h.len(), 2);
        d.add_cell(CellKind::Sql, None, &mut SeededIds(4));
        h.observe(&d, 10_050);
        d.add_cell(CellKind::Sql, None, &mut SeededIds(5));
        h.observe(&d, 10_060);
        assert_eq!(h.len(), 4);
    }

    #[test]
    fn history_is_bounded_and_new_steps_clear_redo() {
        let mut d = doc();
        let mut h = History::with_limit(&d, 3);
        for n in 0..5 {
            d.add_cell(CellKind::Sql, None, &mut SeededIds(10 + n));
            h.observe(&d, 0);
        }
        assert_eq!(h.len(), 3);
        h.undo(&d).unwrap().snapshot.apply_to(&mut d);
        assert!(h.can_redo());
        d.title = "x".into();
        h.observe(&d, 0);
        assert!(!h.can_redo());
    }

    #[test]
    fn untracked_changes_move_the_baseline_without_a_step() {
        let mut d = doc();
        let mut h = History::new(&d);
        d.cells[0].collapsed = true;
        h.note_untracked(&d);
        assert!(!h.observe(&d, 0));
        assert!(!h.can_undo());
    }

    #[test]
    fn snapshots_share_unchanged_cells() {
        let mut d = doc();
        let a = Snapshot::of(&d);
        d.cells[1].source = "# Changed".into();
        let b = Snapshot::capture(&d, &a);
        assert!(Arc::ptr_eq(&a.cells[0], &b.cells[0]));
        assert!(!Arc::ptr_eq(&a.cells[1], &b.cells[1]));
        assert!(Arc::ptr_eq(&a.meta, &b.meta));
    }

    #[test]
    fn undo_keeps_top_level_markers() {
        let mut d = doc();
        let mut h = History::new(&d);
        d.add_cell(CellKind::Sql, None, &mut SeededIds(3));
        h.observe(&d, 0);
        d.extra
            .insert("sqlly_origin".into(), serde_json::json!("machine"));
        h.undo(&d).unwrap().snapshot.apply_to(&mut d);
        assert!(d.extra.contains_key("sqlly_origin"));
    }
}
