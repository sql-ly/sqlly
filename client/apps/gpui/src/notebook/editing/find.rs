//! Find and replace across the notebook: the bar above the cells, match
//! navigation that scrolls the virtualized list (unfolding what hides a
//! match), and replace as one undo step that leaves cells the shell cannot
//! edit untouched.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_services::notebook::editing::find::{
    self as core_find, FindField, FindMatch, FindOptions,
};
use sqlly_services::notebook::references::{self, Dialects};

use super::t;
use crate::icon_catalog::AppIcon;
use crate::notebook::cells;
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

/// Matches listed under the bar (the counter covers all of them).
const LISTED_MATCHES: usize = 50;

pub struct FindState {
    pub replace: bool,
    pub opts: FindOptions,
    pub include_data: bool,
    pub replacement: String,
    pub matches: Vec<FindMatch>,
    pub current: usize,
    query_input: Option<Entity<InputState>>,
    replace_input: Option<Entity<InputState>>,
    stamp: Option<(u64, u64, FindOptions, bool)>,
    focus_pending: bool,
}

impl FindState {
    fn new(replace: bool) -> Self {
        Self {
            replace,
            opts: FindOptions::default(),
            include_data: false,
            replacement: String::new(),
            matches: Vec::new(),
            current: 0,
            query_input: None,
            replace_input: None,
            stamp: None,
            focus_pending: true,
        }
    }
}

/// Whether replace may change `cell`: kinds this build edits, except AI
/// cells, whose text is evidence and stays as the agent wrote it.
fn replaceable(cell: &sqlly_services::notebook::Cell) -> bool {
    let kind = cell.kind.as_str();
    cells::renderer_for(kind).kind() == kind && kind != crate::notebook::ai::KIND_AI
}

impl NotebookView {
    pub(crate) fn open_find(&mut self, replace: bool, cx: &mut Context<Self>) {
        match self.editing.find.as_mut() {
            Some(find) => {
                find.replace |= replace;
                find.focus_pending = true;
            }
            None => self.editing.find = Some(FindState::new(replace)),
        }
        cx.notify();
    }

    pub(crate) fn close_find(&mut self, cx: &mut Context<Self>) {
        self.editing.find = None;
        self.request_focus_root();
        cx.notify();
    }

    /// Set the query (the input does this as the user types).
    pub(crate) fn set_find_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if let Some(find) = self.editing.find.as_mut() {
            find.opts.query = query.to_string();
            find.current = 0;
        }
        self.editing_settle();
        self.refresh_matches();
        cx.notify();
    }

    pub(crate) fn set_find_options(
        &mut self,
        edit: impl FnOnce(&mut FindState),
        cx: &mut Context<Self>,
    ) {
        if let Some(find) = self.editing.find.as_mut() {
            edit(find);
            find.current = 0;
        }
        self.editing_settle();
        self.refresh_matches();
        cx.notify();
    }

    /// Recompute the matches when the document, the outputs or the options
    /// changed since the last search.
    pub(crate) fn refresh_matches(&mut self) {
        let revision = self.editing.revision;
        let generation = self.outputs.generation();
        let Some(find) = self.editing.find.as_ref() else {
            return;
        };
        let stamp = (revision, generation, find.opts.clone(), find.include_data);
        if find.stamp.as_ref() == Some(&stamp) {
            return;
        }
        #[cfg(test)]
        crate::notebook::perf_counters::bump(crate::notebook::perf_counters::Pass::Find);
        let mut matches = core_find::find(self.doc.model(), &find.opts);
        if find.include_data && !find.opts.query.is_empty() {
            for cell in &self.doc.model().cells {
                if let Some(output) = self.outputs.get(&cell.id) {
                    core_find::find_in_rows(
                        &cell.id,
                        &output.columns,
                        &output.rows,
                        &find.opts,
                        &mut matches,
                    );
                }
            }
            let order: std::collections::HashMap<&str, usize> = self
                .doc
                .model()
                .cells
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id.as_str(), i))
                .collect();
            matches.sort_by_key(|m| {
                (
                    m.cell_id
                        .as_deref()
                        .and_then(|id| order.get(id))
                        .map(|i| i + 1)
                        .unwrap_or(0),
                    m.field.clone(),
                    m.start,
                )
            });
        }
        if let Some(find) = self.editing.find.as_mut() {
            find.current = find.current.min(matches.len().saturating_sub(1));
            find.matches = matches;
            find.stamp = Some(stamp);
        }
    }

    pub(crate) fn find_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.editing_settle();
        self.refresh_matches();
        let Some(find) = self.editing.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as isize;
        find.current = (find.current as isize + delta).rem_euclid(len) as usize;
        let at = find.current;
        self.go_to_match(at, cx);
    }

    /// Scroll to match `n` and, in a cell with a live editor, select it.
    pub(crate) fn go_to_match(&mut self, n: usize, cx: &mut Context<Self>) {
        let Some(m) = self
            .editing
            .find
            .as_ref()
            .and_then(|f| f.matches.get(n))
            .cloned()
        else {
            return;
        };
        if let Some(find) = self.editing.find.as_mut() {
            find.current = n;
        }
        let Some(id) = m.cell_id.clone() else {
            cx.notify();
            return;
        };
        self.go_to_cell(&id, cx);
        if m.field == FindField::Source
            && let Some(editor) = self.cells.get(&id).and_then(|rt| rt.editor.clone())
        {
            editor.update(cx, |e, cx| e.adopt_caret_state(m.end, Some(m.start), cx));
        }
        cx.notify();
    }

    pub(crate) fn replace_current(&mut self, cx: &mut Context<Self>) {
        self.editing_settle();
        self.refresh_matches();
        let Some(find) = self.editing.find.as_ref() else {
            return;
        };
        let Some(m) = find.matches.get(find.current).cloned() else {
            return;
        };
        let with = find.replacement.clone();
        let editable = m
            .cell_id
            .as_deref()
            .and_then(|id| self.doc.model().cell(id))
            .is_none_or(replaceable);
        if !m.replaceable() || !editable {
            self.status(t("notebook.editing.find.read_only"), true, cx);
            return;
        }
        let dialects = Dialects::single(self.doc.database_type());
        let renamed = self.change_doc(
            "notebook.editing.step.replace",
            |doc| match core_find::replace_one(doc, &m, &with) {
                Ok(changed) => changed,
                Err((id, name)) => {
                    references::rename_cell(doc, &id, Some(&name), &dialects).is_ok()
                }
            },
            cx,
        );
        if !renamed {
            self.status(t("notebook.editing.find.not_replaced"), true, cx);
        }
        self.refresh_matches();
        if let Some(current) = self.editing.find.as_ref().map(|f| f.current) {
            self.go_to_match(current, cx);
        }
    }

    pub(crate) fn replace_all(&mut self, cx: &mut Context<Self>) {
        self.editing_settle();
        let Some(find) = self.editing.find.as_ref() else {
            return;
        };
        if find.opts.query.is_empty() {
            return;
        }
        let opts = find.opts.clone();
        let with = find.replacement.clone();
        let dialects = Dialects::single(self.doc.database_type());
        let (count, refused) = self.change_doc(
            "notebook.editing.step.replace",
            |doc| {
                let (done, renames) = core_find::replace_all(doc, &opts, &with, &replaceable);
                let mut count = done.count;
                let mut refused = 0;
                for (id, name) in renames {
                    match references::rename_cell(doc, &id, Some(&name), &dialects) {
                        Ok(_) => count += 1,
                        Err(_) => refused += 1,
                    }
                }
                (count, refused)
            },
            cx,
        );
        let mut message =
            crate::localization::format("notebook.editing.find.replaced", &[("count", count)]);
        if refused > 0 {
            message = format!(
                "{message} {}",
                crate::localization::format(
                    "notebook.editing.find.names_refused",
                    &[("count", refused)]
                )
            );
        }
        self.status(message, refused > 0, cx);
        self.refresh_matches();
    }

    fn ensure_find_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = self.editing.find.as_mut() else {
            return;
        };
        if find.query_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "notebook.editing.find.placeholder",
                ))
            });
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let value = input.read(cx).value().to_string();
                    this.set_find_query(&value, cx);
                }
                InputEvent::PressEnter { secondary, .. } => {
                    this.find_step(if *secondary { -1 } else { 1 }, cx);
                }
                _ => {}
            })
            .detach();
            find.query_input = Some(input);
        }
        if find.replace && find.replace_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "notebook.editing.find.replace_placeholder",
                ))
            });
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let value = input.read(cx).value().to_string();
                    if let Some(find) = this.editing.find.as_mut() {
                        find.replacement = value;
                    }
                }
                InputEvent::PressEnter { .. } => this.replace_current(cx),
                _ => {}
            })
            .detach();
            find.replace_input = Some(input);
        }
        if std::mem::take(&mut find.focus_pending)
            && let Some(input) = find.query_input.clone()
        {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    pub(crate) fn render_find_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.editing.find.as_ref()?;
        self.ensure_find_inputs(window, cx);
        self.refresh_matches();
        let find = self.editing.find.as_ref()?;
        let s = Theme::scaled;
        let total = find.matches.len();
        let counter = if find.opts.query.is_empty() {
            String::new()
        } else if total == 0 {
            t("notebook.editing.find.no_matches")
        } else {
            crate::localization::format(
                "notebook.editing.find.counter",
                &[
                    ("current", (find.current + 1).to_string()),
                    ("total", total.to_string()),
                ],
            )
        };
        let toggle = |id: &'static str, label_key: &str, on: bool| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(t(label_key))
                .selected(on)
                .accessibility_label(t(label_key))
        };
        let mut bar = div()
            .id("nb-find-bar")
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(6.0)))
            .border_b_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .aria_label(t("notebook.editing.find.bar"));
        if let Some(input) = &find.query_input {
            bar = bar.child(div().w(px(s(240.0))).child(Input::new(input).small()));
        }
        bar = bar
            .child(
                toggle(
                    "nb-find-case",
                    "notebook.editing.find.match_case",
                    find.opts.case_sensitive,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_find_options(|f| f.opts.case_sensitive = !f.opts.case_sensitive, cx)
                })),
            )
            .child(
                toggle(
                    "nb-find-word",
                    "notebook.editing.find.whole_word",
                    find.opts.whole_word,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_find_options(|f| f.opts.whole_word = !f.opts.whole_word, cx)
                })),
            )
            .child(
                toggle(
                    "nb-find-data",
                    "notebook.editing.find.include_data",
                    find.include_data,
                )
                .tooltip(t("notebook.editing.find.include_data_tooltip"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_find_options(|f| f.include_data = !f.include_data, cx)
                })),
            )
            .child(
                div()
                    .min_w(px(s(70.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(counter),
            )
            .child(
                super::super::rails::icon_button(
                    "nb-find-prev",
                    AppIcon::ChevronUp,
                    t("notebook.editing.find.previous"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.find_step(-1, cx))),
            )
            .child(
                super::super::rails::icon_button(
                    "nb-find-next",
                    AppIcon::ChevronDown,
                    t("notebook.editing.find.next"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.find_step(1, cx))),
            )
            .child(div().flex_1())
            .child(
                super::super::rails::icon_button(
                    "nb-find-close",
                    AppIcon::Close,
                    t("notebook.editing.find.close"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.close_find(cx))),
            );
        let replace_row = find.replace.then(|| {
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .px(px(s(10.0)))
                .pb(px(s(6.0)));
            if let Some(input) = &find.replace_input {
                row = row.child(div().w(px(s(240.0))).child(Input::new(input).small()));
            }
            row.child(
                super::super::rails::text_button(
                    "nb-replace-one",
                    t("notebook.editing.find.replace_one"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.replace_current(cx))),
            )
            .child(
                super::super::rails::text_button(
                    "nb-replace-all",
                    t("notebook.editing.find.replace_all"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.replace_all(cx))),
            )
        });
        let listed: Vec<AnyElement> = find
            .matches
            .iter()
            .enumerate()
            .take(LISTED_MATCHES)
            .map(|(n, m)| {
                let where_ = match (&m.cell_id, &m.field) {
                    (None, _) => t("notebook.editing.find.in_title"),
                    (Some(id), field) => {
                        let name = self
                            .doc
                            .cell_by_id(id)
                            .map(|c| c.display_name().to_string())
                            .unwrap_or_else(|| id.clone());
                        let key = match field {
                            FindField::Name => "notebook.editing.find.in_name",
                            FindField::Spec(_) => "notebook.editing.find.in_settings",
                            FindField::Data { .. } => "notebook.editing.find.in_data",
                            _ => "notebook.editing.find.in_cell",
                        };
                        crate::localization::format(key, &[("cell_name", name.as_str())])
                    }
                };
                let current = n == find.current;
                div()
                    .id(("nb-find-hit", n))
                    .flex()
                    .flex_row()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .cursor_pointer()
                    .when(current, |d| d.bg(Theme::selection_fill()))
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to_match(n, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(where_),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(m.snippet.clone()),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .child(bar)
                .children(replace_row)
                .when(!listed.is_empty(), |d| {
                    d.child(
                        div()
                            .id("nb-find-hits")
                            .max_h(px(s(140.0)))
                            .overflow_y_scroll()
                            .border_b_1()
                            .border_color(Theme::border())
                            .children(listed),
                    )
                })
                .into_any_element(),
        )
    }
}
