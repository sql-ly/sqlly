//! Left rail: Outline, Parameters, Variables (named cells), Lineage and
//! Search, behind one collapsible, resizable divider.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use gpui_component::tab::{Tab, TabBar};

use crate::notebook::layout::LeftRailPage;
use crate::notebook::view::{DragTarget, NotebookView};
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::theme::Theme;

/// Outlines longer than this draw only the rows in view.
const OUTLINE_VIRTUAL_ROWS: usize = 80;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn empty_note(key: &str) -> AnyElement {
    div()
        .px(px(Theme::scaled(10.0)))
        .py(px(Theme::scaled(8.0)))
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(t(key))
        .into_any_element()
}

impl NotebookView {
    fn display_name_of(&self, id: &str) -> String {
        self.doc
            .cell_by_id(id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    fn rail_row(
        &self,
        key: String,
        cell_id: String,
        indent: f32,
        primary: String,
        secondary: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let active = self.active.as_deref() == Some(cell_id.as_str());
        div()
            .id(gpui::ElementId::Name(key.into()))
            .flex()
            .flex_col()
            .pl(px(s(10.0 + indent)))
            .pr(px(s(8.0)))
            .py(px(s(3.0)))
            .cursor_pointer()
            .when(active, |d| d.bg(Theme::selection_fill()))
            .hover(|style| style.bg(Theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.reveal_cell(&cell_id, cx)))
            .child(
                div()
                    .truncate()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(primary),
            )
            .when_some(secondary, |d, text| {
                d.child(
                    div()
                        .truncate()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::text_dim())
                        .child(text),
                )
            })
            .into_any_element()
    }

    fn outline_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let entries = self.doc.outline_shared();
        if entries.is_empty() {
            return vec![empty_note("notebook.shell.rail.outline_empty")];
        }
        if entries.len() > OUTLINE_VIRTUAL_ROWS {
            // A long outline draws only the rows in view, one line each.
            return vec![
                gpui::uniform_list(
                    "nb-outline",
                    entries.len(),
                    cx.processor(
                        move |this: &mut Self, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|n| entries.get(n).cloned().map(|e| (n, e)))
                                .map(|(n, entry)| {
                                    let indent = f32::from(entry.level.saturating_sub(1)) * 10.0;
                                    this.rail_row(
                                        format!("nb-outline-{n}"),
                                        entry.cell_id,
                                        indent,
                                        entry.text,
                                        None,
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        },
                    ),
                )
                .size_full()
                .into_any_element(),
            ];
        }
        entries
            .iter()
            .cloned()
            .enumerate()
            .map(|(n, entry)| {
                let indent = f32::from(entry.level.saturating_sub(1)) * 10.0;
                let secondary = (entry.level == 0).then(|| t("notebook.shell.rail.named_cell"));
                self.rail_row(
                    format!("nb-outline-{n}"),
                    entry.cell_id,
                    indent,
                    entry.text,
                    secondary,
                    cx,
                )
            })
            .collect()
    }

    fn variables_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let named: Vec<(String, String, String)> = self
            .doc
            .cells()
            .filter_map(|c| {
                c.name.map(|name| {
                    (
                        c.id.to_string(),
                        name.to_string(),
                        t(crate::notebook::cells::renderer_for(c.kind).label_key()),
                    )
                })
            })
            .collect();
        if named.is_empty() {
            return vec![empty_note("notebook.shell.rail.variables_empty")];
        }
        named
            .into_iter()
            .enumerate()
            .map(|(n, (id, name, kind))| {
                let state = t(crate::notebook::view::status_label_key(
                    self.cell_state(&id),
                    self.is_stale(&id),
                ));
                let secondary = crate::localization::format(
                    "notebook.shell.rail.variable_detail",
                    &[("kind", kind.as_str()), ("state", state.as_str())],
                );
                self.rail_row(format!("nb-var-{n}"), id, 0.0, name, Some(secondary), cx)
            })
            .collect()
    }

    fn lineage_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rows = self.doc.lineage();
        if rows.is_empty() {
            return vec![empty_note("notebook.shell.rail.lineage_empty")];
        }
        let none = t("notebook.shell.rail.lineage_none");
        rows.into_iter()
            .enumerate()
            .map(|(n, row)| {
                let names = |ids: &[String]| {
                    if ids.is_empty() {
                        none.clone()
                    } else {
                        ids.iter()
                            .map(|id| self.display_name_of(id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                let secondary = crate::localization::format(
                    "notebook.shell.rail.lineage_detail",
                    &[
                        ("reads", names(&row.reads)),
                        ("used_by", names(&row.used_by)),
                    ],
                );
                let primary = self.display_name_of(&row.cell_id);
                self.rail_row(
                    format!("nb-lineage-{n}"),
                    row.cell_id,
                    0.0,
                    primary,
                    Some(secondary),
                    cx,
                )
            })
            .collect()
    }

    fn search_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out: Vec<AnyElement> = Vec::new();
        if let Some(input) = &self.search_input {
            out.push(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .child(Input::new(input).small())
                    .into_any_element(),
            );
        }
        if self.search_query.trim().is_empty() {
            return out;
        }
        let hits = self.doc.search(&self.search_query);
        if hits.is_empty() {
            out.push(empty_note("notebook.shell.rail.search_no_results"));
            return out;
        }
        for (n, hit) in hits.into_iter().enumerate() {
            let primary = crate::localization::format(
                "notebook.shell.rail.search_hit",
                &[
                    ("cell_name", self.display_name_of(&hit.cell_id)),
                    ("line", (hit.line + 1).to_string()),
                ],
            );
            out.push(self.rail_row(
                format!("nb-search-{n}"),
                hit.cell_id,
                0.0,
                primary,
                Some(hit.snippet),
                cx,
            ));
        }
        out
    }

    pub(in crate::notebook) fn render_left_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let page = self.layout.left_page;
        let toggle = this.clone();
        let drag = this.clone();
        let divider = PaneDivider::vertical("nb-left-divider", CollapseDirection::Left)
            .label(t(page.label_key()))
            .collapsed(!self.layout.left_visible)
            .on_toggle(move |collapsed, _, app| {
                toggle.update(app, |v, cx| {
                    v.update_layout(|l| l.left_visible = !collapsed, cx)
                });
            })
            .on_drag_start(move |x, _, app| {
                drag.update(app, |v, _| v.begin_drag(DragTarget::Left, x));
            });
        if !self.layout.left_visible {
            return divider.into_any_element();
        }
        let selected = LeftRailPage::ALL
            .iter()
            .position(|p| *p == page)
            .unwrap_or(0);
        let tabs_this = this.clone();
        let tabs = TabBar::new("nb-left-tabs")
            .underline()
            .selected_index(selected)
            .on_click(move |index: &usize, _, app| {
                if let Some(page) = LeftRailPage::ALL.get(*index).copied() {
                    tabs_this.update(app, |v, cx| v.update_layout(|l| l.left_page = page, cx));
                }
            })
            .children(LeftRailPage::ALL.map(|p| Tab::new().label(t(p.label_key()))));
        let body = match page {
            LeftRailPage::Outline => self.outline_page(cx),
            LeftRailPage::Parameters => self.params_rail_page(window, cx),
            LeftRailPage::Variables => self.variables_page(cx),
            LeftRailPage::Lineage => self.lineage_page(cx),
            LeftRailPage::Search => self.search_page(cx),
            LeftRailPage::LocalData => self.local_data_page(cx),
            LeftRailPage::History => self.history_page(window, cx),
            LeftRailPage::Bookmarks => self.bookmarks_page(cx),
        };
        div()
            .flex()
            .flex_row()
            .h_full()
            .flex_none()
            .child(
                div()
                    .w(px(self.layout.left_width))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(Theme::sidebar_bg())
                    .child(div().px(px(s(4.0))).child(tabs))
                    .child(
                        div()
                            .id("nb-left-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(body),
                    ),
            )
            .child(divider)
            .into_any_element()
    }
}
