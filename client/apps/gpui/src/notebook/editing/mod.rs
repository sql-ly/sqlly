//! Notebook editing: multi-select and bulk actions, the cell clipboard,
//! duplicate, structural undo and redo, block moves (keyboard and drag),
//! split and merge, the insert menu, find and replace, bookmarks, heading
//! folds, pinned outputs, and IntelliSense over referenced cells.
//!
//! Document operations are the core's (`sqlly_services::notebook::editing`);
//! this module applies them to the view through [`NotebookView::change_doc`],
//! which keeps the virtualized list, live editors, cell state and outputs in
//! step and records one undo step per change.

pub(crate) mod clipboard;
mod find;
pub mod intel;
pub mod keys;
pub mod markdown;
mod ui;

#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use gpui::{Context, Window};
use serde_json::Value;
use sqlly_services::notebook::editing::{History, structure};
use sqlly_services::notebook::{CellOutput, CellState, NotebookDoc};

use super::cells;
use super::doc_adapter::{KIND_MARKDOWN, KIND_SQL, RunScope};
use super::reveal::Reveal;
use super::view::{CellRuntime, NotebookView};

pub use find::FindState;

pub use sqlly_services::notebook::editing::{BOOKMARK_KEY, FOLDED_KEY, PINNED_KEY};
/// Removed cells whose state, output and results view are kept for undo.
const MAX_PARKED: usize = 200;

/// Every editing command (keyboard, palette, bulk bar and cell menu).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditingCommand {
    SelectAll,
    ExtendDown,
    ExtendUp,
    ClearSelection,
    Copy,
    Cut,
    PasteBelow,
    PasteAbove,
    Duplicate,
    Undo,
    Redo,
    MoveUp,
    MoveDown,
    ToggleCollapse,
    RunSelected,
    Split,
    MergeBelow,
    MergeAbove,
    InsertMenu,
    Find,
    Replace,
    ToggleBookmark,
    NextBookmark,
    PreviousBookmark,
    ShowBookmarks,
    ToggleFold,
    UnfoldAll,
    TogglePin,
}

/// A removed cell's live parts, kept so undo brings it back as it was.
struct Parked {
    runtime: Option<CellRuntime>,
    state: Option<CellState>,
    output: Option<Arc<CellOutput>>,
}

/// The message under the canvas after a delete, with Undo.
pub struct Toast {
    pub message: String,
}

/// The keyboard insert menu: where it inserts and the highlighted kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertMenu {
    pub index: usize,
    pub highlight: usize,
}

/// A drag of one or more cells by their handle.
#[derive(Debug, Clone)]
pub struct DraggedCells {
    pub ids: Vec<String>,
}

#[derive(Default)]
pub struct EditingState {
    history: Option<History>,
    epoch: Option<Instant>,
    anchor: Option<String>,
    pub toast: Option<Toast>,
    parked: HashMap<String, Parked>,
    parked_order: VecDeque<String>,
    pub(super) find: Option<FindState>,
    /// Cell index and whether the drop lands after it.
    pub(super) drop_target: Option<(usize, bool)>,
    pub(super) insert_menu: Option<InsertMenu>,
    /// Bumped on every document change, and once when typing pauses; keys
    /// the caches below.
    revision: u64,
    /// Text was typed in a cell editor since the revision last moved.
    typed: bool,
    folded_cache: RefCell<Option<(u64, Rc<Vec<bool>>)>>,
    refs_stamp: Option<(u64, u64, usize, u64)>,
    projections: HashMap<String, (u64, Option<Vec<String>>)>,
}

#[cfg(test)]
thread_local! {
    static ITEM_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one cell rendered by the list (perf tests read it).
#[cfg(test)]
pub(super) fn count_item_render() {
    ITEM_RENDERS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
pub(super) fn take_item_renders() -> usize {
    ITEM_RENDERS.with(|c| c.replace(0))
}

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn flag(cell: &sqlly_services::notebook::Cell, key: &str) -> bool {
    cell.extra
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn set_flag(cell: &mut sqlly_services::notebook::Cell, key: &str, on: bool) {
    if on {
        cell.extra.insert(key.to_string(), Value::Bool(true));
    } else {
        cell.extra.remove(key);
    }
}

impl NotebookView {
    // ── Hooks the shell calls ─────────────────────────────────────────────

    pub(super) fn editing_start(&mut self) {
        self.editing.history = Some(History::new(self.doc.model()));
        self.editing.epoch = Some(Instant::now());
    }

    fn now_ms(&self) -> i64 {
        self.editing
            .epoch
            .map(|e| e.elapsed().as_millis() as i64)
            .unwrap_or_default()
    }

    /// Text typed in a cell editor: part of the editor's own undo. The
    /// revision moves once typing pauses ([`Self::editing_settle`]), so a
    /// keystroke doesn't rebuild what is keyed by it.
    pub(super) fn editing_typed(&mut self, id: &str, text: &str) {
        self.editing.typed = true;
        if let Some(h) = self.editing.history.as_mut() {
            h.note_typed(id, text);
        }
    }

    /// Fold text typed since the last change into the revision; true when
    /// there was some.
    pub(super) fn editing_settle(&mut self) -> bool {
        let typed = std::mem::take(&mut self.editing.typed);
        if typed {
            self.editing.revision += 1;
        }
        typed
    }

    /// Record whatever changed since the last step as one undo step.
    pub(super) fn editing_observe(&mut self) {
        self.editing.typed = false;
        self.editing.revision += 1;
        let now = self.now_ms();
        if let Some(h) = self.editing.history.as_mut() {
            #[cfg(test)]
            super::perf_counters::bump(super::perf_counters::Pass::History);
            h.observe(self.doc.model(), now);
        }
    }

    /// Name the next undo step (`notebook.editing.step.*`).
    pub(super) fn editing_label(&mut self, key: &'static str) {
        if let Some(h) = self.editing.history.as_mut() {
            h.label_next(key);
        }
    }

    /// The change just made (a bookmark, fold or pin) is not an undo step.
    fn editing_untracked(&mut self) {
        self.editing.revision += 1;
        if let Some(h) = self.editing.history.as_mut() {
            h.note_untracked(self.doc.model());
        }
    }

    #[cfg(test)]
    pub fn can_undo_structure(&self) -> bool {
        self.editing.history.as_ref().is_some_and(History::can_undo)
    }

    #[cfg(test)]
    pub fn can_redo_structure(&self) -> bool {
        self.editing.history.as_ref().is_some_and(History::can_redo)
    }

    // ── Applying document changes ─────────────────────────────────────────

    fn park(&mut self, id: String, state: Option<CellState>) {
        let runtime = self.cells.remove(&id).map(|mut rt| {
            rt.drop_editor();
            rt
        });
        let output = self.outputs.get(&id);
        self.outputs.remove(&id);
        self.editing.parked.insert(
            id.clone(),
            Parked {
                runtime,
                state,
                output,
            },
        );
        self.editing.parked_order.push_back(id);
        while self.editing.parked_order.len() > MAX_PARKED {
            if let Some(old) = self.editing.parked_order.pop_front() {
                self.editing.parked.remove(&old);
            }
        }
    }

    fn unpark(&mut self, id: &str) {
        let Some(parked) = self.editing.parked.remove(id) else {
            return;
        };
        self.editing.parked_order.retain(|p| p != id);
        if let Some(rt) = parked.runtime {
            self.cells.entry(id.to_string()).or_insert(rt);
        }
        if let Some(state) = parked.state {
            let was_active = state.status.is_active();
            self.doc.put_state(id, state);
            if was_active {
                self.doc.apply(
                    id,
                    sqlly_services::notebook::CellEvent::Canceled {
                        server_confirmed: false,
                    },
                );
            }
        }
        if let Some(output) = parked.output {
            self.outputs.publish(id, (*output).clone());
        }
    }

    /// Apply `edit` to the document as one undo step named `label`, then
    /// bring the view in line: list items spliced, removed cells parked
    /// (their state, output and results kept for undo), returning cells
    /// restored, editors of changed cells given the new text, editors of
    /// cells whose kind changed dropped, selection pruned.
    pub(crate) fn change_doc<R>(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut NotebookDoc) -> R,
        cx: &mut Context<Self>,
    ) -> R {
        let before: Vec<(String, String)> = self
            .doc
            .cells()
            .map(|c| (c.id.to_string(), c.kind.to_string()))
            .collect();
        let (result, removed) = self.doc.edit_model_parking(edit);
        let removed: HashMap<String, CellState> = removed.into_iter().collect();
        let after: Vec<(String, String)> = self
            .doc
            .cells()
            .map(|c| (c.id.to_string(), c.kind.to_string()))
            .collect();
        let after_ids: HashSet<&str> = after.iter().map(|(id, _)| id.as_str()).collect();
        let before_ids: HashSet<&str> = before.iter().map(|(id, _)| id.as_str()).collect();
        let mut removed = removed;
        let gone: Vec<String> = before
            .iter()
            .filter(|(id, _)| !after_ids.contains(id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        self.forget_run_steps(&gone, cx);
        for id in &gone {
            let state = removed.remove(id);
            self.park(id.clone(), state);
        }
        let returning: Vec<String> = after
            .iter()
            .filter(|(id, _)| !before_ids.contains(id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &returning {
            self.unpark(id);
        }
        let kinds_before: HashMap<&str, &str> = before
            .iter()
            .map(|(i, k)| (i.as_str(), k.as_str()))
            .collect();
        for (id, kind) in &after {
            if kinds_before.get(id.as_str()).is_some_and(|k| *k != kind)
                && let Some(rt) = self.cells.get_mut(id)
            {
                rt.drop_editor();
            }
        }
        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(a, b)| a.0 == b.0)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(a, b)| a.0 == b.0)
            .count();
        self.list_state
            .splice(prefix..before.len() - suffix, after.len() - suffix - prefix);
        self.list_state.remeasure();
        let live: Vec<(String, gpui::Entity<crate::editor::SqlEditor>)> = self
            .cells
            .iter()
            .filter_map(|(id, rt)| rt.editor.clone().map(|e| (id.clone(), e)))
            .collect();
        for (id, editor) in live {
            if let Some(source) = self.doc.cell_by_id(&id).map(|c| c.source.to_string())
                && editor.read(cx).text() != source
            {
                editor.update(cx, |e, cx| e.replace_document(source, cx));
            }
        }
        self.selected.retain(|id| after_ids.contains(id.as_str()));
        if self
            .active
            .as_deref()
            .is_some_and(|a| !after_ids.contains(a))
        {
            self.active = after.first().map(|(id, _)| id.clone());
        }
        if self
            .editing_markdown
            .as_deref()
            .is_some_and(|m| !after_ids.contains(m))
        {
            self.editing_markdown = None;
        }
        self.name_input = None;
        self.editing_label(label);
        self.mark_dirty(cx);
        result
    }

    // ── Undo and redo ─────────────────────────────────────────────────────

    fn restore_step(&mut self, redo: bool, cx: &mut Context<Self>) {
        if self.is_running() {
            self.status(t("notebook.editing.status.busy"), true, cx);
            return;
        }
        self.editing_observe();
        let Some(history) = self.editing.history.as_mut() else {
            return;
        };
        let restore = if redo {
            history.redo(self.doc.model())
        } else {
            history.undo(self.doc.model())
        };
        let Some(restore) = restore else {
            self.status(
                t(if redo {
                    "notebook.editing.status.nothing_to_redo"
                } else {
                    "notebook.editing.status.nothing_to_undo"
                }),
                false,
                cx,
            );
            return;
        };
        let title_before = self.doc.title().unwrap_or_default().to_string();
        let before: HashSet<String> = self.doc.cells().map(|c| c.id.to_string()).collect();
        self.change_doc(restore.label, |doc| restore.snapshot.apply_to(doc), cx);
        let returned = self
            .doc
            .cells()
            .map(|c| c.id.to_string())
            .find(|id| !before.contains(id));
        if let Some(id) = returned {
            self.activate_cell(&id, cx);
        }
        self.ai.forget_applied();
        self.editing.toast = None;
        self.param_ui.reset_widgets();
        let title = self.doc.title().unwrap_or_default().to_string();
        if title != title_before {
            self.title_input = None;
            cx.emit(super::view::NotebookEvent::TitleChanged(title));
        }
        self.doc.refresh_staleness();
        let message = crate::localization::format(
            if redo {
                "notebook.editing.status.redone"
            } else {
                "notebook.editing.status.undone"
            },
            &[("step", t(restore.label))],
        );
        self.status(message, false, cx);
        if let Some(active) = self.active.clone() {
            self.reveal(&active, Reveal::Select, cx);
        }
    }

    pub(crate) fn undo_structure(&mut self, cx: &mut Context<Self>) {
        self.restore_step(false, cx);
    }

    pub(crate) fn redo_structure(&mut self, cx: &mut Context<Self>) {
        self.restore_step(true, cx);
    }

    // ── Selection ─────────────────────────────────────────────────────────

    /// What bulk actions act on: the selected cells in document order, else
    /// the active cell.
    pub(crate) fn selection(&self) -> Vec<String> {
        if self.selected.is_empty() {
            return self.active.iter().cloned().collect();
        }
        let chosen: HashSet<&str> = self.selected.iter().map(String::as_str).collect();
        self.doc
            .cells()
            .filter(|c| chosen.contains(c.id))
            .map(|c| c.id.to_string())
            .collect()
    }

    fn select_range(&mut self, from: &str, to: &str) {
        let (Some(a), Some(b)) = (self.doc.index_of(from), self.doc.index_of(to)) else {
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let hidden = self.folded_away();
        self.selected = (lo..=hi)
            .filter(|i| !hidden.get(*i).copied().unwrap_or(false))
            .filter_map(|i| self.doc.cell(i).map(|c| c.id.to_string()))
            .collect();
    }

    /// A click on a cell: Shift extends the selection from the anchor, the
    /// platform modifier toggles the cell, a plain click selects it alone.
    pub(crate) fn click_cell(
        &mut self,
        id: &str,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if modifiers.shift {
            let anchor = self
                .editing
                .anchor
                .clone()
                .or_else(|| self.active.clone())
                .unwrap_or_else(|| id.to_string());
            self.editing.anchor = Some(anchor.clone());
            self.select_range(&anchor, id);
            self.activate_cell(id, cx);
        } else if crate::keybindings::primary_modifier(&modifiers) {
            if self.selected.is_empty()
                && let Some(active) = self.active.clone()
                && active != id
            {
                self.selected.push(active);
            }
            self.toggle_selected(id, cx);
            self.editing.anchor = Some(id.to_string());
            self.activate_cell(id, cx);
        } else {
            if !self.selected.is_empty() {
                self.selected.clear();
            }
            self.editing.anchor = Some(id.to_string());
            self.activate_cell(id, cx);
        }
    }

    fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(active) = self.active.clone() else {
            return;
        };
        let anchor = self
            .editing
            .anchor
            .clone()
            .filter(|a| !self.selected.is_empty() && self.doc.index_of(a).is_some())
            .unwrap_or_else(|| active.clone());
        self.editing.anchor = Some(anchor.clone());
        let Some(at) = self.doc.index_of(&active) else {
            return;
        };
        let next = (at as isize + delta).clamp(0, self.doc.len() as isize - 1) as usize;
        let next = self.skip_folded(at, next, delta);
        let Some(next_id) = self.doc.cell(next).map(|c| c.id.to_string()) else {
            return;
        };
        self.select_range(&anchor, &next_id);
        self.activate_cell(&next_id, cx);
        self.reveal(&next_id, Reveal::Select, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = self.doc.cells().map(|c| c.id.to_string()).collect();
        self.editing.anchor = self.selected.first().cloned();
        cx.notify();
    }

    /// Esc in command mode: the insert menu, then the find bar, then the
    /// selection.
    fn escape(&mut self, cx: &mut Context<Self>) {
        if self.editing.insert_menu.take().is_some() {
        } else if self.editing.find.is_some() {
            self.close_find(cx);
        } else {
            self.selected.clear();
            self.editing.anchor = None;
        }
        cx.notify();
    }

    // ── Bulk actions ──────────────────────────────────────────────────────

    /// The selection without cells that are running (they cannot change).
    fn changeable_selection(&self) -> Vec<String> {
        self.selection()
            .into_iter()
            .filter(|id| !self.doc.status(id).is_active())
            .collect()
    }

    pub(crate) fn delete_cells(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        self.delete_cells_as(ids, "notebook.editing.step.delete", cx);
    }

    pub(crate) fn delete_cells_as(
        &mut self,
        ids: Vec<String>,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.doc.index_of(id).is_some() && !self.doc.status(id).is_active())
            .collect();
        if ids.is_empty() {
            return;
        }
        let first = ids
            .iter()
            .filter_map(|id| self.doc.index_of(id))
            .min()
            .unwrap_or(0);
        let message = if ids.len() == 1 {
            let name = self
                .doc
                .cell_by_id(&ids[0])
                .map(|c| c.display_name().to_string())
                .unwrap_or_default();
            crate::localization::format(
                "notebook.shell.cell.deleted_toast",
                &[("cell_name", name.as_str())],
            )
        } else {
            crate::localization::format("notebook.editing.toast.deleted", &[("count", ids.len())])
        };
        let gone: HashSet<String> = ids.iter().cloned().collect();
        self.change_doc(label, |doc| doc.cells.retain(|c| !gone.contains(&c.id)), cx);
        self.active = self
            .doc
            .cell(first.min(self.doc.len().saturating_sub(1)))
            .map(|c| c.id.to_string());
        self.selected.clear();
        self.editing.toast = Some(Toast { message });
        cx.notify();
    }

    pub(crate) fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selection();
        self.delete_cells(ids, cx);
    }

    pub(crate) fn convert_selection(&mut self, kind: &str, cx: &mut Context<Self>) {
        let ids = self.changeable_selection();
        if ids.len() <= 1 {
            if let Some(id) = ids.first() {
                self.convert_cell(id, kind, cx);
            }
            return;
        }
        let target = sqlly_services::notebook::CellKind::parse(kind);
        self.change_doc(
            "notebook.editing.step.convert",
            |doc| {
                for cell in doc.cells.iter_mut().filter(|c| ids.contains(&c.id)) {
                    let convertible = |k: &sqlly_services::notebook::CellKind| {
                        matches!(k.as_str(), KIND_SQL | KIND_MARKDOWN)
                    };
                    if convertible(&cell.kind) && convertible(&target) {
                        cell.kind = target.clone();
                    }
                }
            },
            cx,
        );
    }

    fn collapse_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selection();
        let collapse = ids
            .iter()
            .any(|id| self.doc.cell_by_id(id).is_some_and(|c| !c.collapsed));
        self.change_doc(
            "notebook.editing.step.collapse",
            |doc| {
                for cell in doc.cells.iter_mut().filter(|c| ids.contains(&c.id)) {
                    cell.collapsed = collapse;
                }
            },
            cx,
        );
    }

    /// Move the selection one cell up or down as a block.
    pub(crate) fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids = self.selection();
        let indexes: Vec<usize> = ids.iter().filter_map(|id| self.doc.index_of(id)).collect();
        let (Some(lo), Some(hi)) = (indexes.iter().min(), indexes.iter().max()) else {
            return;
        };
        let to = if delta < 0 {
            if *lo == 0 {
                return;
            }
            lo - 1
        } else {
            if hi + 1 >= self.doc.len() {
                return;
            }
            lo + 1
        };
        self.move_block(ids, to, cx);
    }

    /// Move `ids` as a block to start at `to` (index without the block).
    pub(crate) fn move_block(&mut self, ids: Vec<String>, to: usize, cx: &mut Context<Self>) {
        let moved = self.change_doc(
            "notebook.editing.step.move",
            |doc| structure::move_block(doc, &ids, to),
            cx,
        );
        if moved && let Some(active) = self.active.clone() {
            self.reveal(&active, Reveal::Select, cx);
        }
    }

    fn run_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selection();
        if ids.is_empty() {
            return;
        }
        let kept = std::mem::replace(&mut self.selected, ids);
        self.run_scope(RunScope::Selected, None, cx);
        self.selected = kept;
    }

    fn duplicate_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selection();
        let Some(last) = ids.iter().filter_map(|id| self.doc.index_of(id)).max() else {
            return;
        };
        let fragment = sqlly_services::notebook::editing::Fragment::of(self.doc.model(), &ids);
        self.paste_fragment(fragment, last + 1, "notebook.editing.step.duplicate", cx);
    }

    // ── Split and merge ───────────────────────────────────────────────────

    fn split_active(&mut self, window: Option<&Window>, cx: &mut Context<Self>) {
        let id = window
            .and_then(|w| self.focused_cell(w, cx))
            .or_else(|| self.active.clone());
        let Some(id) = id else {
            return;
        };
        let Some(editor) = self.cells.get(&id).and_then(|rt| rt.editor.clone()) else {
            self.status(t("notebook.editing.status.split_needs_cursor"), true, cx);
            return;
        };
        let cursor = editor.read(cx).cursor();
        let new_id = self.change_doc(
            "notebook.editing.step.split",
            |doc| structure::split_cell(doc, &id, cursor, &mut sqlly_services::notebook::RandomIds),
            cx,
        );
        match new_id {
            Some(new_id) => {
                self.edit_cell(&new_id, cx);
            }
            None => self.status(t("notebook.editing.status.split_refused"), true, cx),
        }
    }

    fn merge(&mut self, below: bool, cx: &mut Context<Self>) {
        let mut ids = self.changeable_selection();
        if ids.len() == 1 {
            let Some(at) = self.doc.index_of(&ids[0]) else {
                return;
            };
            let other = if below {
                at.checked_add(1)
            } else {
                at.checked_sub(1)
            };
            match other
                .and_then(|i| self.doc.cell(i))
                .map(|c| c.id.to_string())
                .filter(|other| !self.doc.status(other).is_active())
            {
                Some(other) if below => ids.push(other),
                Some(other) => ids.insert(0, other),
                None => return,
            }
        }
        let merged = self.change_doc(
            "notebook.editing.step.merge",
            |doc| structure::merge_cells(doc, &ids),
            cx,
        );
        match merged {
            Some(_) => {
                self.selected.clear();
                if let Some(first) = ids.first() {
                    self.activate_cell(first, cx);
                }
            }
            None => self.status(t("notebook.editing.status.merge_refused"), true, cx),
        }
    }

    // ── Bookmarks, folds and pins ─────────────────────────────────────────

    fn set_flags(&mut self, ids: &[String], key: &str, on: bool, cx: &mut Context<Self>) {
        let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.doc.edit_model(|doc| {
            for cell in doc.cells.iter_mut().filter(|c| ids.contains(c.id.as_str())) {
                set_flag(cell, key, on);
            }
        });
        self.editing_untracked();
        self.list_state.remeasure();
        self.mark_dirty(cx);
    }

    pub(crate) fn cell_flag(&self, id: &str, key: &str) -> bool {
        self.doc.model().cell(id).is_some_and(|c| flag(c, key))
    }

    pub(crate) fn toggle_bookmark(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let on = ids.iter().any(|id| !self.cell_flag(id, BOOKMARK_KEY));
        self.set_flags(&ids, BOOKMARK_KEY, on, cx);
    }

    pub(crate) fn bookmarked(&self) -> Vec<String> {
        self.doc
            .model()
            .cells
            .iter()
            .filter(|c| flag(c, BOOKMARK_KEY))
            .map(|c| c.id.clone())
            .collect()
    }

    fn jump_bookmark(&mut self, forward: bool, cx: &mut Context<Self>) {
        let marks: Vec<usize> = self
            .bookmarked()
            .iter()
            .filter_map(|id| self.doc.index_of(id))
            .collect();
        if marks.is_empty() {
            self.status(t("notebook.editing.status.no_bookmarks"), false, cx);
            return;
        }
        let at = self.active.as_deref().and_then(|a| self.doc.index_of(a));
        let next = match (forward, at) {
            (true, Some(at)) => marks.iter().find(|m| **m > at).or(marks.first()),
            (false, Some(at)) => marks.iter().rev().find(|m| **m < at).or(marks.last()),
            (true, None) => marks.first(),
            (false, None) => marks.last(),
        };
        if let Some(id) = next
            .and_then(|i| self.doc.cell(*i))
            .map(|c| c.id.to_string())
        {
            self.go_to_cell(&id, cx);
        }
    }

    /// Reveal `id`, unfolding the sections that hide it.
    pub(crate) fn go_to_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.doc.index_of(id)
            && self.is_folded_away(index)
        {
            let model = self.doc.model();
            let headings: Vec<String> = (0..index)
                .filter(|h| {
                    flag(&model.cells[*h], FOLDED_KEY)
                        && structure::section(model, *h).contains(&index)
                })
                .map(|h| model.cells[h].id.clone())
                .collect();
            self.set_flags(&headings, FOLDED_KEY, false, cx);
        }
        self.reveal_cell(id, cx);
    }

    /// The heading a fold command acts on: the active cell when it is a
    /// heading, else the nearest heading above it.
    fn fold_target(&self) -> Option<String> {
        let model = self.doc.model();
        let at = self.active.as_deref().and_then(|a| model.cell_index(a))?;
        (0..=at)
            .rev()
            .find(|i| structure::heading_level(&model.cells[*i]).is_some())
            .map(|i| model.cells[i].id.clone())
    }

    pub(crate) fn toggle_fold(&mut self, heading: Option<String>, cx: &mut Context<Self>) {
        let Some(heading) = heading.or_else(|| self.fold_target()) else {
            self.status(t("notebook.editing.status.no_heading"), false, cx);
            return;
        };
        let on = !self.cell_flag(&heading, FOLDED_KEY);
        self.set_flags(std::slice::from_ref(&heading), FOLDED_KEY, on, cx);
        if on {
            self.activate_cell(&heading, cx);
            self.selected.clear();
        }
    }

    fn unfold_all(&mut self, cx: &mut Context<Self>) {
        let folded: Vec<String> = self
            .doc
            .model()
            .cells
            .iter()
            .filter(|c| flag(c, FOLDED_KEY))
            .map(|c| c.id.clone())
            .collect();
        self.set_flags(&folded, FOLDED_KEY, false, cx);
    }

    /// Which cells folded headings hide, by index (cached per revision).
    pub(crate) fn folded_away(&self) -> Rc<Vec<bool>> {
        let revision = self.editing.revision;
        if let Some((r, hidden)) = self.editing.folded_cache.borrow().as_ref()
            && *r == revision
            && hidden.len() == self.doc.len()
        {
            return hidden.clone();
        }
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::Folds);
        let model = self.doc.model();
        let hidden = if model.cells.iter().any(|c| flag(c, FOLDED_KEY)) {
            structure::hidden_by_folds(model, &|c| flag(c, FOLDED_KEY))
        } else {
            vec![false; model.cells.len()]
        };
        let hidden = Rc::new(hidden);
        *self.editing.folded_cache.borrow_mut() = Some((revision, hidden.clone()));
        hidden
    }

    pub(crate) fn is_folded_away(&self, index: usize) -> bool {
        self.folded_away().get(index).copied().unwrap_or(false)
    }

    /// Cells a folded heading hides, counted for its chip.
    pub(crate) fn folded_count(&self, index: usize) -> usize {
        structure::section(self.doc.model(), index).len()
    }

    /// `next`, or the nearest visible cell beyond it in the direction of
    /// travel (keyboard navigation skips folded cells).
    pub(super) fn skip_folded(&self, from: usize, next: usize, delta: isize) -> usize {
        let hidden = self.folded_away();
        if !hidden.get(next).copied().unwrap_or(false) {
            return next;
        }
        let mut i = next as isize;
        while i >= 0 && (i as usize) < hidden.len() {
            if !hidden[i as usize] {
                return i as usize;
            }
            i += delta.signum();
        }
        from
    }

    pub(crate) fn toggle_pin(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let on = ids.iter().any(|id| !self.cell_flag(id, PINNED_KEY));
        self.set_flags(&ids, PINNED_KEY, on, cx);
    }

    pub(crate) fn pinned(&self) -> Vec<String> {
        self.doc
            .model()
            .cells
            .iter()
            .filter(|c| flag(c, PINNED_KEY))
            .map(|c| c.id.clone())
            .collect()
    }

    // ── Insert menu ───────────────────────────────────────────────────────

    fn insert_kinds() -> Vec<&'static (dyn cells::NotebookCell + Sync)> {
        cells::insertable_kinds().collect()
    }

    fn open_insert_menu(&mut self, cx: &mut Context<Self>) {
        let index = match self.active.as_deref().and_then(|a| self.doc.index_of(a)) {
            Some(at) => at + 1,
            None => self.doc.len(),
        };
        self.editing.insert_menu = Some(InsertMenu {
            index,
            highlight: 0,
        });
        cx.notify();
    }

    /// j/k and the arrows move the insert menu's highlight while it is open.
    pub(super) fn insert_menu_step(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.editing.insert_menu.as_mut() else {
            return false;
        };
        let count = Self::insert_kinds().len() as isize;
        menu.highlight = (menu.highlight as isize + delta).rem_euclid(count.max(1)) as usize;
        cx.notify();
        true
    }

    /// Enter inserts the highlighted kind while the insert menu is open.
    pub(super) fn insert_menu_confirm(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.editing.insert_menu.take() else {
            return false;
        };
        if let Some(renderer) = Self::insert_kinds().get(menu.highlight) {
            self.insert_kind_at(renderer.kind(), menu.index, cx);
        }
        true
    }

    /// Insert a `kind` cell at `index` and start editing it.
    pub(crate) fn insert_kind_at(&mut self, kind: &str, index: usize, cx: &mut Context<Self>) {
        self.editing.insert_menu = None;
        let id = self.insert_cell(kind, index, cx);
        self.reveal(&id, Reveal::Select, cx);
        self.selected.clear();
        self.edit_cell(&id, cx);
    }

    // ── Commands ──────────────────────────────────────────────────────────

    pub fn editing_command(&mut self, command: EditingCommand, cx: &mut Context<Self>) {
        self.editing_command_in(command, None, cx);
    }

    pub(crate) fn editing_command_in(
        &mut self,
        command: EditingCommand,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.present.takes_over() {
            return;
        }
        use EditingCommand as C;
        match command {
            C::SelectAll => self.select_all(cx),
            C::ExtendDown => self.extend_selection(1, cx),
            C::ExtendUp => self.extend_selection(-1, cx),
            C::ClearSelection => self.escape(cx),
            C::Copy => self.copy_selection(false, cx),
            C::Cut => self.copy_selection(true, cx),
            C::PasteBelow => self.paste_from_clipboard(true, cx),
            C::PasteAbove => self.paste_from_clipboard(false, cx),
            C::Duplicate => self.duplicate_selection(cx),
            C::Undo => self.undo_structure(cx),
            C::Redo => self.redo_structure(cx),
            C::MoveUp => self.move_selection(-1, cx),
            C::MoveDown => self.move_selection(1, cx),
            C::ToggleCollapse => self.collapse_selection(cx),
            C::RunSelected => self.run_selection(cx),
            C::Split => self.split_active(window.as_deref(), cx),
            C::MergeBelow => self.merge(true, cx),
            C::MergeAbove => self.merge(false, cx),
            C::InsertMenu => self.open_insert_menu(cx),
            C::Find => self.open_find(false, cx),
            C::Replace => self.open_find(true, cx),
            C::ToggleBookmark => {
                let ids = self.selection();
                self.toggle_bookmark(ids, cx);
            }
            C::NextBookmark => self.jump_bookmark(true, cx),
            C::PreviousBookmark => self.jump_bookmark(false, cx),
            C::ShowBookmarks => self.update_layout(
                |l| {
                    l.left_visible = true;
                    l.left_page = super::layout::LeftRailPage::Bookmarks;
                },
                cx,
            ),
            C::ToggleFold => self.toggle_fold(None, cx),
            C::UnfoldAll => self.unfold_all(cx),
            C::TogglePin => {
                let ids = self.selection();
                self.toggle_pin(ids, cx);
            }
        }
    }

    // ── IntelliSense contexts ─────────────────────────────────────────────

    /// Give every live editor its view of the other cells, when outputs,
    /// the document or the set of editors changed since the last push.
    pub(super) fn refresh_reference_contexts(&mut self, cx: &mut Context<Self>) {
        let live = self.cells.values().filter(|rt| rt.editor.is_some()).count();
        let stamp = (
            self.outputs.generation(),
            self.editing.revision,
            live,
            self.doc.generation(),
        );
        if live == 0 || self.editing.refs_stamp == Some(stamp) {
            return;
        }
        self.editing.refs_stamp = Some(stamp);
        let entries = self.reference_entries();
        let params = self.param_entries();
        let attachments = self.attachment_entries();
        let slash = ui::slash_items();
        let editors: Vec<(String, gpui::Entity<crate::editor::SqlEditor>)> = self
            .cells
            .iter()
            .filter_map(|(id, rt)| rt.editor.clone().map(|e| (id.clone(), e)))
            .collect();
        for (id, editor) in editors {
            let markdown = self
                .doc
                .cell_by_id(&id)
                .is_some_and(|c| c.kind == KIND_MARKDOWN);
            // A Markdown cell may name itself (it stays as written); a SQL
            // cell lists only the others.
            let ctx = intel::ReferenceContext {
                cells: entries
                    .iter()
                    .filter(|e| markdown || e.id != id)
                    .cloned()
                    .collect(),
                params: params.clone(),
                slash: if markdown { slash.clone() } else { Vec::new() },
                attachments: if markdown {
                    attachments.clone()
                } else {
                    Vec::new()
                },
                param_kind_label: t("notebook.editing.intel.parameter"),
                from_output_note: t("notebook.editing.intel.columns_from_output"),
                from_query_note: t("notebook.editing.intel.columns_from_query"),
            };
            editor.update(cx, |e, cx| {
                e.set_notebook_references(Some(Arc::new(ctx)), cx)
            });
        }
    }

    fn reference_entries(&mut self) -> Vec<intel::ReferenceEntry> {
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::References);
        let mut out = Vec::with_capacity(self.doc.len());
        let cells: Vec<(String, String, String, Option<String>)> = self
            .doc
            .cells()
            .map(|c| {
                (
                    c.id.to_string(),
                    c.kind.to_string(),
                    c.source.to_string(),
                    c.name.map(str::to_string),
                )
            })
            .collect();
        let mut seen = HashSet::new();
        for (id, kind, source, name) in cells {
            seen.insert(id.clone());
            let renderer = cells::renderer_for(&kind);
            let (columns, from_output) = match self.outputs.get(&id) {
                Some(output) => (
                    output
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.data_type.clone()))
                        .collect(),
                    true,
                ),
                None if kind == KIND_SQL => (
                    self.projection(&id, &source)
                        .into_iter()
                        .map(|n| (n, String::new()))
                        .collect(),
                    false,
                ),
                None => (Vec::new(), false),
            };
            let status = self.doc.status(&id);
            let stale = self.is_stale(&id);
            let state_label = if renderer.runnable() || renderer.runs_locally() {
                t(super::view::status_label_key(status, stale))
            } else {
                String::new()
            };
            let shows_in_markdown = kind == "scalar";
            let (markdown_value, value_note) = if shows_in_markdown {
                let partial = self.outputs.get(&id).is_some_and(|o| {
                    sqlly_services::notebook::cells::scalar::is_partial(o.completeness)
                });
                let note = if stale {
                    Some(t("notebook.shell.state.stale"))
                } else if partial {
                    Some(t("notebook.markdown_editor.intel.partial"))
                } else if matches!(
                    status,
                    sqlly_services::notebook::CellStatus::Fresh
                        | sqlly_services::notebook::CellStatus::Cached
                ) {
                    None
                } else {
                    Some(state_label.clone())
                };
                (self.scalar_display(&id), note)
            } else {
                (None, None)
            };
            out.push(intel::ReferenceEntry {
                token: name.unwrap_or_else(|| id.clone()),
                id,
                kind_label: t(renderer.label_key()),
                state_label,
                columns,
                columns_from_output: from_output,
                shows_in_markdown,
                markdown_value,
                value_note,
            });
        }
        self.editing.projections.retain(|id, _| seen.contains(id));
        out
    }

    /// The column names a SQL cell's query projects, when statically known
    /// (cached by source).
    fn projection(&mut self, id: &str, source: &str) -> Vec<String> {
        let hash = super::preview::source_hash(source);
        if let Some((h, names)) = self.editing.projections.get(id)
            && *h == hash
        {
            return names.clone().unwrap_or_default();
        }
        let names = sqlly_intel::query::column_provenance::StatementProvenance::parse(source)
            .and_then(|p| p.output_names());
        self.editing
            .projections
            .insert(id.to_string(), (hash, names.clone()));
        names.unwrap_or_default()
    }

    fn param_entries(&self) -> Vec<intel::ParamEntry> {
        use sqlly_services::notebook::ParamType;
        use sqlly_services::notebook::cells::parameter::display_value;
        let resolved = self.doc.resolved();
        self.doc
            .model()
            .parameters
            .iter()
            .map(|p| intel::ParamEntry {
                sensitive: p.is_redacted(),
                value: (!p.is_redacted())
                    .then(|| resolved.get(&p.name).and_then(|v| display_value(v, None)))
                    .flatten(),
                name: p.name.clone(),
                type_label: if p.label.is_empty() {
                    p.ty.kind().to_string()
                } else {
                    format!("{} · {}", p.label, p.ty.kind())
                },
                fields: if matches!(p.ty, ParamType::DateRange) {
                    vec!["start".into(), "end".into(), "end_exclusive".into()]
                } else {
                    Vec::new()
                },
            })
            .collect()
    }

    /// The attachment cells a Markdown picture can name.
    fn attachment_entries(&self) -> Vec<intel::AttachmentEntry> {
        self.doc
            .cells()
            .filter(|c| c.kind == super::doc_adapter::KIND_ATTACHMENT)
            .filter_map(|c| {
                let spec = self.doc.attachment(c.id)?;
                Some(intel::AttachmentEntry {
                    token: c.name.unwrap_or(c.id).to_string(),
                    id: c.id.to_string(),
                    file_name: spec.file_name.clone().unwrap_or_default(),
                    picture: spec.is_image(),
                })
            })
            .collect()
    }

    /// A slash-menu item was accepted in a Markdown cell's editor.
    pub(crate) fn slash_accepted(&mut self, cell_id: &str, label: &str, cx: &mut Context<Self>) {
        let action = self
            .cells
            .get(cell_id)
            .and_then(|rt| rt.editor.clone())
            .and_then(|e| e.read(cx).notebook_references())
            .and_then(|ctx| ctx.slash_action(label));
        if let Some(intel::SlashAction::InsertCell(kind)) = action
            && let Some(at) = self.doc.index_of(cell_id)
        {
            self.insert_kind_at(kind, at + 1, cx);
        }
    }

    /// A short spoken description of the cell at `index`: kind, name,
    /// state and position.
    pub(crate) fn cell_accessibility_label(&self, index: usize) -> String {
        let Some(cell) = self.doc.cell(index) else {
            return String::new();
        };
        let renderer = cells::renderer_for(cell.kind);
        let state = if renderer.runnable() {
            t(super::view::status_label_key(
                self.doc.status(cell.id),
                self.is_stale(cell.id),
            ))
        } else {
            String::new()
        };
        let mut label = crate::localization::format(
            "notebook.editing.a11y.cell",
            &[
                ("kind", t(renderer.label_key())),
                ("cell_name", cell.display_name().to_string()),
                ("position", (index + 1).to_string()),
                ("count", self.doc.len().to_string()),
            ],
        );
        if !state.is_empty() {
            label = format!("{label}, {state}");
        }
        if self.selected.iter().any(|s| s == cell.id) {
            label = format!("{label}, {}", t("notebook.editing.a11y.selected"));
        }
        label
    }
}
