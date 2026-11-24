//! What editing adds to the notebook's surface: the bulk bar, the delete
//! toast, drag handles with the drop indicator and edge auto-scroll, the
//! inline insert row between cells and the keyboard insert menu, header
//! badges and the cell menu, the pinned outputs strip, the Bookmarks rail
//! page and the Markdown slash menu's items.

use gpui::{
    AnyElement, AppContext as _, Context, CursorStyle, DragMoveEvent, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Stateful, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use sqlly_services::notebook::editing::find::value_text;
use sqlly_services::notebook::editing::structure;

use super::intel::{SlashAction, SlashItem, SnippetScope};
use super::{BOOKMARK_KEY, DraggedCells, EditingCommand, FOLDED_KEY, PINNED_KEY, t};
use crate::icon_catalog::AppIcon;
use crate::notebook::cells::{self, CellRenderCx};
use crate::notebook::rails::{icon_button, quiet_text, text_button};
use crate::notebook::view::{Hovered, NotebookView};
use crate::theme::Theme;

/// Pinned outputs shown at once in the strip.
const MAX_PINNED: usize = 6;
/// Rows and columns of a pinned output's preview.
const PIN_ROWS: usize = 3;
const PIN_COLUMNS: usize = 5;
/// Distance from the canvas edge where a drag scrolls the list.
const EDGE: f32 = 56.0;

struct DragPreview(String);

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(Theme::scaled(4.0)))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::accent())
            .text_color(Theme::text())
            .child(self.0.clone())
    }
}

/// A translated word as snippet placeholder text (escaped for `${1:…}`).
fn placeholder(key: &str) -> String {
    t(key)
        .replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('}', "\\}")
}

/// The Markdown snippets: the slash menu shows all of them, and Ctrl+Space
/// in a Markdown cell offers the block, inline and reference ones.
pub(super) fn slash_items() -> Vec<SlashItem> {
    let format_detail = t("notebook.editing.slash.format_detail");
    let text = |key: &str, insert: &str| SlashItem {
        label: t(key),
        detail: format_detail.clone(),
        insert: insert.to_string(),
        action: None,
        scope: SnippetScope::Block,
    };
    let inline = |key: &str, insert: String| SlashItem {
        detail: t("notebook.markdown_editor.snippet.inline_detail"),
        scope: SnippetScope::Inline,
        ..text(key, &insert)
    };
    let reference = |key: &str, insert: &str| SlashItem {
        detail: t("notebook.editing.slash.reference_detail"),
        scope: SnippetScope::Reference,
        ..text(key, insert)
    };
    let word = placeholder("notebook.markdown_editor.snippet.placeholder_text");
    let column = placeholder("notebook.markdown_editor.snippet.table_column");
    let mut items = vec![
        text("notebook.editing.slash.heading_1", "# "),
        text("notebook.editing.slash.heading_2", "## "),
        text("notebook.editing.slash.heading_3", "### "),
        text("notebook.editing.slash.bulleted_list", "- "),
        text("notebook.editing.slash.numbered_list", "1. "),
        text("notebook.editing.slash.checklist", "- [ ] "),
        text(
            "notebook.editing.slash.table",
            &format!("| ${{1:{column}}} | ${{2:{column}}} |\n| --- | --- |\n| $3 |  |\n"),
        ),
        SlashItem {
            insert: format!("> **{}:** ", t("notebook.editing.slash.callout_word")),
            ..text("notebook.editing.slash.callout", "")
        },
        text("notebook.editing.slash.quote", "> "),
        text("notebook.editing.slash.divider", "---\n"),
        text("notebook.editing.slash.code", "```${1:sql}\n$0\n```"),
        inline(
            "notebook.markdown_editor.snippet.bold",
            format!("**${{1:{word}}}**"),
        ),
        inline(
            "notebook.markdown_editor.snippet.italic",
            format!("_${{1:{word}}}_"),
        ),
        inline(
            "notebook.markdown_editor.snippet.inline_code",
            format!(
                "`${{1:{}}}`",
                placeholder("notebook.markdown_editor.snippet.placeholder_code")
            ),
        ),
        inline(
            "notebook.markdown_editor.snippet.link",
            format!("[${{1:{word}}}]($2)"),
        ),
        inline(
            "notebook.markdown_editor.snippet.image",
            format!(
                "![${{1:{}}}](attachment:$2)",
                placeholder("notebook.markdown_editor.snippet.placeholder_caption")
            ),
        ),
        reference("notebook.editing.slash.reference", "{{"),
        reference("notebook.editing.slash.parameter", "{{param."),
    ];
    for renderer in cells::insertable_kinds() {
        items.push(SlashItem {
            label: crate::localization::format(
                "notebook.editing.slash.new_cell",
                &[("kind", t(renderer.label_key()))],
            ),
            detail: t("notebook.editing.slash.new_cell_detail"),
            insert: String::new(),
            action: Some(SlashAction::InsertCell(renderer.kind())),
            scope: SnippetScope::Cell,
        });
    }
    items
}

fn small(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(Theme::text_dim())
        .child(text)
}

fn badge(text: String) -> AnyElement {
    let s = Theme::scaled;
    div()
        .px(px(s(5.0)))
        .rounded(px(s(3.0)))
        .bg(Theme::input_bg())
        .text_size(px(Theme::font_size_ratio(0.75)))
        .text_color(Theme::accent())
        .child(text)
        .into_any_element()
}

impl NotebookView {
    pub(crate) fn render_editing_toast(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toast = self.editing.toast.as_ref()?;
        let s = Theme::scaled;
        Some(
            div()
                .id("nb-undo-toast")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .mx(px(s(12.0)))
                .mb(px(s(8.0)))
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .rounded(px(s(4.0)))
                .border_1()
                .border_color(Theme::border())
                .bg(Theme::popup_bg())
                .role(gpui::Role::Status)
                .child(
                    div()
                        .flex_1()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(toast.message.clone()),
                )
                .child(
                    text_button("nb-undo", t("notebook.shell.cell.undo_delete"))
                        .on_click(cx.listener(|this, _, _, cx| this.undo_delete(cx))),
                )
                .child(
                    icon_button(
                        "nb-undo-dismiss",
                        AppIcon::Close,
                        t("notebook.shell.cell.dismiss_toast"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss_undo(cx))),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_bulk_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.selected.len() < 2 {
            return None;
        }
        let s = Theme::scaled;
        let count = self.selection().len();
        let action =
            |id: &'static str, key: &str, command: EditingCommand, cx: &mut Context<Self>| {
                text_button(id, t(key))
                    .on_click(cx.listener(move |this, _, _, cx| this.editing_command(command, cx)))
                    .into_any_element()
            };
        let convert = |id: &'static str, key: &str, kind: &'static str, cx: &mut Context<Self>| {
            text_button(id, t(key))
                .on_click(cx.listener(move |this, _, _, cx| this.convert_selection(kind, cx)))
                .into_any_element()
        };
        let buttons = vec![
            action(
                "nb-bulk-run",
                "notebook.editing.bulk.run",
                EditingCommand::RunSelected,
                cx,
            ),
            action(
                "nb-bulk-copy",
                "notebook.editing.bulk.copy",
                EditingCommand::Copy,
                cx,
            ),
            action(
                "nb-bulk-cut",
                "notebook.editing.bulk.cut",
                EditingCommand::Cut,
                cx,
            ),
            action(
                "nb-bulk-duplicate",
                "notebook.editing.bulk.duplicate",
                EditingCommand::Duplicate,
                cx,
            ),
            action(
                "nb-bulk-up",
                "notebook.editing.bulk.move_up",
                EditingCommand::MoveUp,
                cx,
            ),
            action(
                "nb-bulk-down",
                "notebook.editing.bulk.move_down",
                EditingCommand::MoveDown,
                cx,
            ),
            action(
                "nb-bulk-collapse",
                "notebook.editing.bulk.collapse",
                EditingCommand::ToggleCollapse,
                cx,
            ),
            convert(
                "nb-bulk-markdown",
                "notebook.editing.bulk.to_markdown",
                crate::notebook::doc_adapter::KIND_MARKDOWN,
                cx,
            ),
            convert(
                "nb-bulk-sql",
                "notebook.editing.bulk.to_sql",
                crate::notebook::doc_adapter::KIND_SQL,
                cx,
            ),
            action(
                "nb-bulk-merge",
                "notebook.editing.bulk.merge",
                EditingCommand::MergeBelow,
                cx,
            ),
            action(
                "nb-bulk-bookmark",
                "notebook.editing.bulk.bookmark",
                EditingCommand::ToggleBookmark,
                cx,
            ),
            text_button("nb-bulk-delete", t("notebook.editing.bulk.delete"))
                .on_click(cx.listener(|this, _, _, cx| this.delete_selection(cx)))
                .into_any_element(),
            action(
                "nb-bulk-clear",
                "notebook.editing.bulk.clear",
                EditingCommand::ClearSelection,
                cx,
            ),
        ];
        Some(
            div()
                .id("nb-bulk-bar")
                .flex()
                .flex_row()
                .flex_wrap()
                .flex_none()
                .items_center()
                .gap(px(s(4.0)))
                .px(px(s(10.0)))
                .py(px(s(4.0)))
                .border_b_1()
                .border_color(Theme::border())
                .bg(Theme::selection_fill())
                .role(gpui::Role::Toolbar)
                .aria_label(t("notebook.editing.bulk.toolbar"))
                .child(
                    div()
                        .mr(px(s(6.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "notebook.editing.bulk.selected",
                            &[("count", count)],
                        )),
                )
                .children(buttons)
                .into_any_element(),
        )
    }

    /// The handle a cell is dragged by (the whole selection when the cell
    /// is part of it).
    pub(crate) fn render_drag_handle(
        &mut self,
        cell: &CellRenderCx,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let ids = if self.selected.contains(&cell.id) {
            self.selection()
        } else {
            vec![cell.id.clone()]
        };
        let name = self
            .doc
            .cell_by_id(&cell.id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_default();
        let label = if ids.len() > 1 {
            crate::localization::format("notebook.editing.drag.cells", &[("count", ids.len())])
        } else {
            name.clone()
        };
        div()
            .id(("nb-drag", cell.index))
            .flex_none()
            .px(px(s(1.0)))
            .cursor(CursorStyle::OpenHand)
            .text_color(Theme::text_dim())
            .hover(|style| style.text_color(Theme::text()))
            .role(gpui::Role::Button)
            .aria_label(crate::localization::format(
                "notebook.editing.drag.handle",
                &[("cell_name", name.as_str())],
            ))
            .aria_description(t("notebook.editing.drag.keyboard_hint"))
            .child(
                Icon::empty()
                    .path(AppIcon::MoreVertical.path())
                    .size(px(s(14.0))),
            )
            .on_drag(DraggedCells { ids }, move |_, _, _, cx| {
                let label = label.clone();
                cx.new(|_| DragPreview(label))
            })
            .into_any_element()
    }

    pub(crate) fn editing_header_badges(
        &mut self,
        cell: &CellRenderCx,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.cell_flag(&cell.id, BOOKMARK_KEY) {
            out.push(badge(t("notebook.editing.badge.bookmarked")));
        }
        if self.cell_flag(&cell.id, PINNED_KEY) {
            out.push(badge(t("notebook.editing.badge.pinned")));
        }
        let heading = self
            .doc
            .model()
            .cell(&cell.id)
            .and_then(structure::heading_level)
            .is_some();
        if heading {
            let folded = self.cell_flag(&cell.id, FOLDED_KEY);
            let label = if folded {
                crate::localization::format(
                    "notebook.editing.fold.unfold_count",
                    &[("count", self.folded_count(cell.index))],
                )
            } else {
                t("notebook.editing.fold.fold")
            };
            if !self.shows_controls(&cell.id) {
                out.push(quiet_text(label, cx).into_any_element());
                return out;
            }
            let id = cell.id.clone();
            out.push(
                text_button(format!("nb-fold-{}", cell.index), label)
                    .tooltip(t("notebook.editing.fold.tooltip"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_fold(Some(id.clone()), cx)),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// The `⋯` menu on every cell header.
    pub(crate) fn editing_cell_menu(
        &mut self,
        cell: &CellRenderCx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity();
        let id = cell.id.clone();
        let bookmarked = self.cell_flag(&id, BOOKMARK_KEY);
        let pinned = self.cell_flag(&id, PINNED_KEY);
        let multi = self.selected.len() >= 2 && self.selected.contains(&id);
        let kind = cell.kind.clone();
        let splittable = matches!(
            kind.as_str(),
            crate::notebook::doc_adapter::KIND_SQL | crate::notebook::doc_adapter::KIND_MARKDOWN
        );
        let embed = self.embedded_menu(cell);
        icon_button(
            format!("nb-cell-menu-{}", cell.index),
            AppIcon::MoreHorizontal,
            t("notebook.editing.menu.label"),
        )
        .dropdown_menu(move |menu: PopupMenu, _, _| {
            let item = |menu: PopupMenu, key: &str, command: EditingCommand| {
                let this = this.clone();
                let id = id.clone();
                menu.item(PopupMenuItem::new(t(key)).on_click(move |_, _, app| {
                    this.update(app, |v, cx| {
                        v.target_cell_for_menu(&id, cx);
                        v.editing_command(command, cx);
                    });
                }))
            };
            let mut menu = item(menu, "notebook.editing.menu.copy", EditingCommand::Copy);
            menu = item(menu, "notebook.editing.menu.cut", EditingCommand::Cut);
            menu = item(
                menu,
                "notebook.editing.menu.duplicate",
                EditingCommand::Duplicate,
            );
            menu = item(
                menu,
                "notebook.editing.menu.paste_below",
                EditingCommand::PasteBelow,
            );
            menu = menu.separator();
            if splittable {
                menu = item(menu, "notebook.editing.menu.split", EditingCommand::Split);
                menu = item(
                    menu,
                    "notebook.editing.menu.merge_below",
                    EditingCommand::MergeBelow,
                );
                menu = item(
                    menu,
                    "notebook.editing.menu.merge_above",
                    EditingCommand::MergeAbove,
                );
                menu = menu.separator();
            }
            menu = item(
                menu,
                if bookmarked {
                    "notebook.editing.menu.remove_bookmark"
                } else {
                    "notebook.editing.menu.bookmark"
                },
                EditingCommand::ToggleBookmark,
            );
            menu = item(
                menu,
                if pinned {
                    "notebook.editing.menu.unpin"
                } else {
                    "notebook.editing.menu.pin"
                },
                EditingCommand::TogglePin,
            );
            menu = item(
                menu,
                "notebook.editing.menu.fold",
                EditingCommand::ToggleFold,
            );
            menu = embed.add_to(menu, &this);
            if multi {
                menu = item(
                    menu.separator(),
                    "notebook.editing.menu.run_selected",
                    EditingCommand::RunSelected,
                );
            }
            menu
        })
        .into_any_element()
    }

    /// Make the cell a menu was opened on the target of its command (the
    /// selection when it is part of one).
    pub(crate) fn target_cell_for_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.selected.iter().any(|s| s == id) {
            self.selected.clear();
        }
        self.activate_cell(id, cx);
    }

    /// The list item around a cell: drop indicator, drag targeting, the
    /// spoken description, the inline insert row and the insert menu.
    pub(crate) fn editing_wrap_cell(
        &mut self,
        ix: usize,
        cell: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let dragging = cx.has_active_drag();
        let indicator = self
            .editing
            .drop_target
            .filter(|(at, _)| dragging && *at == ix)
            .map(|(_, after)| after);
        let gap = if self.present.dense { 6.0 } else { 10.0 };
        let group = format!("nb-gap-{ix}");
        let insert_at = ix + 1;
        let this = cx.entity();
        // The insert button exists only in the gap the pointer is on.
        let hovered = self.hovered == Some(Hovered::Gap(ix));
        #[cfg(test)]
        if hovered {
            crate::notebook::perf_counters::bump(crate::notebook::perf_counters::Pass::Button);
        }
        let plus = hovered.then(|| {
            Button::new(gpui::ElementId::Name(format!("nb-insert-{ix}").into()))
                .ghost()
                .xsmall()
                .icon(Icon::empty().path(AppIcon::Add.path()))
                .accessibility_label(t("notebook.editing.insert.button"))
                .tooltip(t("notebook.editing.insert.button"))
                .dropdown_menu(move |mut menu: PopupMenu, _, _| {
                    for renderer in cells::insertable_kinds() {
                        let this = this.clone();
                        let kind = renderer.kind();
                        menu = menu.item(PopupMenuItem::new(t(renderer.label_key())).on_click(
                            move |_, _, app| {
                                this.update(app, |v, cx| v.insert_kind_at(kind, insert_at, cx));
                            },
                        ));
                    }
                    menu
                })
        });
        let menu = self
            .editing
            .insert_menu
            .filter(|m| m.index == insert_at)
            .map(|m| self.render_insert_menu(m.highlight, cx));
        let line = |top: bool| {
            div()
                .absolute()
                .left(px(s(8.0)))
                .right(px(s(8.0)))
                .h(px(s(2.0)))
                .bg(Theme::accent())
                .when(top, |d| d.top_0())
                .when(!top, |d| d.bottom(px(s(gap))))
        };
        div()
            .id(("nb-item", ix))
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .role(gpui::Role::Group)
            .aria_label(self.cell_accessibility_label(ix))
            .on_drag_move::<DraggedCells>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedCells>, _, cx| {
                    let position = event.event.position;
                    if !event.bounds.contains(&position) {
                        return;
                    }
                    let after = position.y > event.bounds.center().y;
                    if this.editing.drop_target != Some((ix, after)) {
                        this.editing.drop_target = Some((ix, after));
                        cx.notify();
                    }
                },
            ))
            .child(cell)
            .children(menu)
            .child(
                div()
                    .id(gpui::ElementId::Name(group.clone().into()))
                    .group(group.clone())
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.hover(Hovered::Gap(ix), cx);
                        }
                    }))
                    .h(px(s(gap)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .opacity(0.0)
                            .group_hover(group, |style| style.opacity(1.0))
                            .children(plus),
                    ),
            )
            .when_some(indicator, |d, after| d.child(line(!after)))
            .into_any_element()
    }

    fn render_insert_menu(&mut self, highlight: usize, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let rows: Vec<AnyElement> = cells::insertable_kinds()
            .enumerate()
            .map(|(n, renderer)| {
                let kind = renderer.kind();
                let index = self
                    .editing
                    .insert_menu
                    .map(|m| m.index)
                    .unwrap_or(self.doc.len());
                div()
                    .id(("nb-insert-kind", n))
                    .px(px(s(10.0)))
                    .py(px(s(3.0)))
                    .cursor_pointer()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .when(n == highlight, |d| d.bg(Theme::selection_fill()))
                    .hover(|style| style.bg(Theme::row_hover()))
                    .role(gpui::Role::MenuItem)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.insert_kind_at(kind, index, cx)),
                    )
                    .child(t(renderer.label_key()))
                    .into_any_element()
            })
            .collect();
        div()
            .id("nb-insert-menu")
            .mx(px(s(24.0)))
            .my(px(s(4.0)))
            .py(px(s(4.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::accent())
            .bg(Theme::popup_bg())
            .role(gpui::Role::Menu)
            .aria_label(t("notebook.editing.insert.menu"))
            .child(
                div()
                    .px(px(s(10.0)))
                    .pb(px(s(2.0)))
                    .child(small(t("notebook.editing.insert.hint"))),
            )
            .children(rows)
            .into_any_element()
    }

    /// The canvas around the list: drag auto-scroll near the edges and the
    /// drop itself.
    pub(crate) fn editing_canvas(
        &mut self,
        canvas: Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        canvas
            .on_drag_move::<DraggedCells>(cx.listener(
                |this, event: &DragMoveEvent<DraggedCells>, _, cx| {
                    let y = event.event.position.y;
                    let top = event.bounds.top();
                    let bottom = event.bounds.bottom();
                    if y < top || y > bottom {
                        return;
                    }
                    let reduce_motion = cx.reduce_motion();
                    let step = |distance: f32| {
                        if reduce_motion {
                            EDGE
                        } else {
                            (EDGE - distance).max(0.0) / EDGE * 24.0 + 2.0
                        }
                    };
                    let from_top = f32::from(y - top);
                    let from_bottom = f32::from(bottom - y);
                    if from_top < EDGE {
                        this.list_state.scroll_by(px(-step(from_top)));
                        cx.notify();
                    } else if from_bottom < EDGE {
                        this.list_state.scroll_by(px(step(from_bottom)));
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, dragged: &DraggedCells, _, cx| {
                let target = match this.editing.drop_target.take() {
                    Some((ix, after)) => ix + usize::from(after),
                    None => this.doc.len(),
                };
                this.drop_cells(dragged.ids.clone(), target, cx);
            }))
    }

    /// Drop `ids` before the cell at `target` (an index in the full list).
    pub(crate) fn drop_cells(&mut self, ids: Vec<String>, target: usize, cx: &mut Context<Self>) {
        let to = structure::drop_index(self.doc.model(), &ids, target.min(self.doc.len()));
        self.move_block(ids, to, cx);
        cx.notify();
    }

    pub(crate) fn render_pinned_strip(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pinned = self.pinned();
        if pinned.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let cards: Vec<AnyElement> = pinned
            .into_iter()
            .take(MAX_PINNED)
            .enumerate()
            .map(|(n, id)| {
                let name = self
                    .doc
                    .cell_by_id(&id)
                    .map(|c| c.display_name().to_string())
                    .unwrap_or_default();
                let state = t(crate::notebook::view::status_label_key(
                    self.doc.status(&id),
                    self.is_stale(&id),
                ));
                let body = match self.outputs.get(&id) {
                    Some(output) => {
                        let columns: Vec<String> = output
                            .columns
                            .iter()
                            .take(PIN_COLUMNS)
                            .map(|c| c.name.clone())
                            .collect();
                        let mut grid = div().flex().flex_col().child(
                            div().flex().flex_row().gap(px(s(8.0))).children(
                                columns
                                    .iter()
                                    .map(|c| small(c.clone()).w(px(s(70.0))).truncate()),
                            ),
                        );
                        for row in output.rows.iter().take(PIN_ROWS) {
                            grid = grid.child(div().flex().flex_row().gap(px(s(8.0))).children(
                                row.iter().take(PIN_COLUMNS).map(|v| {
                                    div()
                                        .w(px(s(70.0)))
                                        .truncate()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text())
                                        .child(value_text(v).unwrap_or_default())
                                }),
                            ));
                        }
                        let rows = crate::localization::format(
                            "notebook.editing.pin.rows",
                            &[("count", output.row_count)],
                        );
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(2.0)))
                            .child(grid)
                            .child(small(rows))
                    }
                    None => div().child(small(t("notebook.editing.pin.no_output"))),
                };
                let go = id.clone();
                let unpin = id.clone();
                div()
                    .id(("nb-pin", n))
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .min_w(px(s(220.0)))
                    .max_w(px(s(420.0)))
                    .p(px(s(6.0)))
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .bg(Theme::panel_bg())
                    .role(gpui::Role::Group)
                    .aria_label(crate::localization::format(
                        "notebook.editing.pin.card",
                        &[("cell_name", name.as_str())],
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                div()
                                    .id(("nb-pin-go", n))
                                    .cursor_pointer()
                                    .text_size(px(Theme::font_size_ratio(0.9)))
                                    .text_color(Theme::text())
                                    .child(name.clone())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.go_to_cell(&go, cx)),
                                    ),
                            )
                            .child(small(state))
                            .child(div().flex_1())
                            .child(
                                icon_button(
                                    format!("nb-unpin-{n}"),
                                    AppIcon::Close,
                                    t("notebook.editing.menu.unpin"),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.toggle_pin(vec![unpin.clone()], cx),
                                )),
                            ),
                    )
                    .child(body)
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("nb-pinned-strip")
                .flex()
                .flex_row()
                .flex_none()
                .gap(px(s(8.0)))
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .max_h(px(s(150.0)))
                .overflow_x_scroll()
                .border_b_1()
                .border_color(Theme::border())
                .bg(Theme::sidebar_bg())
                .role(gpui::Role::Region)
                .aria_label(t("notebook.editing.pin.strip"))
                .children(cards)
                .into_any_element(),
        )
    }

    pub(crate) fn bookmarks_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let marks = self.bookmarked();
        if marks.is_empty() {
            return vec![
                div()
                    .px(px(s(10.0)))
                    .py(px(s(8.0)))
                    .child(small(t("notebook.editing.bookmarks.empty")))
                    .into_any_element(),
            ];
        }
        marks
            .into_iter()
            .enumerate()
            .map(|(n, id)| {
                let (name, kind, first_line) = self
                    .doc
                    .cell_by_id(&id)
                    .map(|c| {
                        (
                            c.display_name().to_string(),
                            t(cells::renderer_for(c.kind).label_key()),
                            c.source
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("")
                                .trim()
                                .chars()
                                .take(80)
                                .collect::<String>(),
                        )
                    })
                    .unwrap_or_default();
                let active = self.active.as_deref() == Some(id.as_str());
                let go = id.clone();
                div()
                    .id(("nb-bookmark", n))
                    .flex()
                    .flex_col()
                    .px(px(s(10.0)))
                    .py(px(s(3.0)))
                    .cursor_pointer()
                    .when(active, |d| d.bg(Theme::selection_fill()))
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to_cell(&go, cx)))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text())
                            .child(format!("{name} · {kind}")),
                    )
                    .child(small(first_line).truncate())
                    .into_any_element()
            })
            .collect()
    }
}
