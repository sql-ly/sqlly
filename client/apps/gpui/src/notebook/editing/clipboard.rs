//! Copy, cut and paste of whole cells. The clipboard carries plain text for
//! other apps (a SQL script when every copied cell is SQL, else Markdown
//! with fenced SQL) and the native fragment as its metadata; pasting reads
//! the fragment first, then a fragment pasted as text, then plain text split
//! into cells.

use gpui::{ClipboardItem, Context};
use sqlly_services::notebook::editing::{Fragment, fragment};
use sqlly_services::notebook::{Cell, CellKind, RandomIds};

use super::t;
use crate::notebook::doc_adapter::KIND_SQL;
use crate::notebook::reveal::Reveal;
use crate::notebook::view::NotebookView;

/// The clipboard item for `fragment`: plain text plus the fragment.
pub fn clipboard_item(
    fragment: &Fragment,
    db: sqlly_lang::sqlmodel::DatabaseType,
) -> ClipboardItem {
    let all_sql = fragment.cells.iter().all(|c| c.kind.as_str() == KIND_SQL);
    let text = if all_sql {
        fragment::sql_text(fragment, db)
    } else {
        fragment::markdown_text(fragment)
    };
    ClipboardItem::new_string_with_metadata(text, fragment.to_json())
}

/// What a clipboard item pastes as.
pub fn fragment_from_clipboard(
    item: &ClipboardItem,
    db: sqlly_lang::sqlmodel::DatabaseType,
    policy: &sqlly_services::notebook::ExecutionPolicy,
) -> Option<Fragment> {
    if let Some(fragment) = item.metadata().and_then(|m| Fragment::from_json(m)) {
        return Some(fragment);
    }
    let text = item.text()?;
    if let Some(fragment) = Fragment::from_json(&text) {
        return Some(fragment);
    }
    let family = db.family();
    let split = move |sql: &str| -> Vec<String> {
        crate::editor::statements::statement_spans_for(sql, family)
            .into_iter()
            .map(|span| sql[span.start..span.end].to_string())
            .collect()
    };
    let cells: Vec<Cell> = fragment::cells_from_text(&text, &split)
        .into_iter()
        .enumerate()
        .map(|(n, (kind, source))| {
            let mut cell = Cell::new(format!("pasted_{n}"), kind, policy.clone());
            cell.source = source;
            cell
        })
        .collect();
    (!cells.is_empty()).then(|| Fragment {
        format: fragment::FRAGMENT_FORMAT.to_string(),
        format_version: fragment::FRAGMENT_VERSION,
        source_notebook: None,
        connection_refs: Vec::new(),
        cells,
    })
}

impl NotebookView {
    pub(crate) fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let ids = self.selection();
        if ids.is_empty() {
            return;
        }
        let fragment = Fragment::of(self.doc.model(), &ids);
        cx.write_to_clipboard(clipboard_item(&fragment, self.doc.database_type()));
        let count = fragment.cells.len();
        if cut {
            self.delete_cells_as(ids, "notebook.editing.step.cut", cx);
        } else {
            self.status(
                crate::localization::format("notebook.editing.status.copied", &[("count", count)]),
                false,
                cx,
            );
        }
    }

    /// Paste below (or above) the active cell.
    pub(crate) fn paste_from_clipboard(&mut self, below: bool, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let policy = self.doc.model().settings.default_policy.clone();
        let Some(fragment) = fragment_from_clipboard(&item, self.doc.database_type(), &policy)
        else {
            self.status(t("notebook.editing.status.nothing_to_paste"), false, cx);
            return;
        };
        let index = match self.active.as_deref().and_then(|a| self.doc.index_of(a)) {
            Some(at) if below => at + 1,
            Some(at) => at,
            None => self.doc.len(),
        };
        self.paste_fragment(fragment, index, "notebook.editing.step.paste", cx);
    }

    /// Paste `fragment` at `index` as one undo step and select the copies.
    pub(crate) fn paste_fragment(
        &mut self,
        fragment: Fragment,
        index: usize,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if fragment.is_empty() {
            return;
        }
        let dialects = sqlly_services::notebook::Dialects::single(self.doc.database_type());
        let ids = self.change_doc(
            label,
            |doc| fragment::paste(doc, &fragment, index, &dialects, &mut RandomIds),
            cx,
        );
        let Some(first) = ids.first().cloned() else {
            return;
        };
        self.selected = if ids.len() > 1 {
            ids.clone()
        } else {
            Vec::new()
        };
        self.activate_cell(&first, cx);
        self.reveal(&first, Reveal::Select, cx);
        let writes = fragment
            .cells
            .iter()
            .any(|c| c.kind == CellKind::Sql && !c.execution_policy.read_only);
        let mut message =
            crate::localization::format("notebook.editing.status.pasted", &[("count", ids.len())]);
        if writes {
            message = format!(
                "{message} {}",
                t("notebook.editing.status.pasted_read_only")
            );
        }
        self.status(message, false, cx);
    }
}
