//! The formula bar over a result grid — Excel's `fx` bar for `-- sqlly
//! column:` computed columns.
//!
//! Opening it (header menu ▸ Add Computed Column… / Edit Formula…) shows a
//! name and a formula field above the grid. While the formula parses, every
//! reference is drawn in its own color, and the grid outlines the cells it
//! reads for the selected row (and tints the columns) in the same colors —
//! the way Excel shows a formula's inputs while you type it. Selecting a
//! cell of a computed column shows its formula read-only with the same
//! coloring. Apply computes the column right away and has the host write
//! the directive into the query.

use std::collections::HashMap;

use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Window, anchored,
    deferred, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{
    Enter, Escape, IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use sqlly_datatable::ReferenceHighlights;

use crate::computed_columns::{ComputedColumn, ComputedPlan, Slot};
use crate::formula::{self, Formula, ParseError, RefSpan};
use crate::theme::Theme;
use crate::tooltip::AppTooltip as _;

use super::{ResultsView, ResultsViewEvent, grid_marker_width};

/// How much larger than the stock small controls the formula editor draws
/// its inputs, buttons and text, so formulas are comfortable to read and edit.
const EDITOR_GROW: f32 = 1.3;

/// A metric `v` (unscaled px) at the formula editor's enlarged size.
fn grown(v: f32) -> gpui::Pixels {
    px(Theme::scaled(v) * EDITOR_GROW)
}

/// A small `Input` resized by [`EDITOR_GROW`]: the stock small input is
/// 24px tall with `text_sm` (0.875rem) text.
fn grown_input(state: &Entity<InputState>) -> Input {
    Input::new(state)
        .small()
        .h(grown(24.0))
        .text_size(gpui::rems(0.875 * EDITOR_GROW))
}

/// Excel's reference colors, in the order it hands them out.
const REFERENCE_COLORS: [(u8, u8, u8); 8] = [
    (0x44, 0x72, 0xC4),
    (0xE0, 0x44, 0x3E),
    (0x80, 0x64, 0xA2),
    (0x3F, 0x9A, 0x3A),
    (0xF7, 0x96, 0x46),
    (0x2C, 0xA6, 0xA4),
    (0xC0, 0x38, 0x9A),
    (0x9C, 0x65, 0x00),
];

fn reference_color(i: usize) -> Hsla {
    let (r, g, b) = REFERENCE_COLORS[i % REFERENCE_COLORS.len()];
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// One color per distinct reference (the same `!Qty[1]` twice shares one),
/// in order of first appearance — Excel's rule.
pub(crate) fn reference_colors(refs: &[RefSpan]) -> Vec<Hsla> {
    let mut seen: Vec<(String, formula::RowRef)> = Vec::new();
    refs.iter()
        .map(|r| {
            let key = (r.column.to_ascii_lowercase(), r.row);
            let i = seen.iter().position(|k| *k == key).unwrap_or_else(|| {
                seen.push(key);
                seen.len() - 1
            });
            reference_color(i)
        })
        .collect()
}

/// An open formula bar.
pub(crate) struct FormulaBar {
    slot: usize,
    /// The computed column being edited (by name), `None` for a new one.
    replacing: Option<String>,
    after: Option<String>,
    name: String,
    formula: String,
    name_input: Option<Entity<InputState>>,
    formula_input: Option<Entity<InputState>>,
    error: Option<String>,
    /// The highlighted completion.
    completion_index: usize,
    /// Escape closed the completion list; typing reopens it.
    completions_hidden: bool,
    _subscriptions: Vec<Subscription>,
}

impl FormulaBar {
    fn parsed(&self) -> Result<Formula, ParseError> {
        formula::parse(&self.formula)
    }
}

impl ResultsView {
    /// Open the formula bar on `slot`. `column` is the clicked header: a
    /// computed column opens for editing, any other seeds a new column that
    /// reads it and sits right after it.
    pub fn open_formula_bar(
        &mut self,
        slot: usize,
        column: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let editing = column.as_ref().and_then(|name| {
            self.computed_specs
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
                .cloned()
        });
        let (replacing, name, formula, after) = match editing {
            Some(spec) => (
                Some(spec.name.clone()),
                spec.name,
                format!(
                    "={}",
                    spec.formula.source.trim().trim_start_matches('=').trim()
                ),
                spec.after,
            ),
            None => {
                let name = self.unused_column_name(slot);
                let formula = match &column {
                    Some(c) => format!("={}", reference_text(c)),
                    None => "=".to_string(),
                };
                (None, name, formula, column)
            }
        };
        self.formula_bar = Some(FormulaBar {
            slot,
            replacing,
            after,
            name,
            formula,
            name_input: None,
            formula_input: None,
            error: None,
            completion_index: 0,
            completions_hidden: false,
            _subscriptions: Vec::new(),
        });
        self.sync_formula_highlights(slot, cx);
        cx.notify();
    }

    /// Start a new computed column placed after `column`, its formula
    /// seeded with a reference to it.
    pub fn open_formula_bar_after(
        &mut self,
        slot: usize,
        column: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let name = self.unused_column_name(slot);
        let formula = column
            .as_ref()
            .map(|c| format!("={}", reference_text(c)))
            .unwrap_or_else(|| "=".into());
        self.formula_bar = Some(FormulaBar {
            slot,
            replacing: None,
            after: column,
            name,
            formula,
            name_input: None,
            formula_input: None,
            error: None,
            completion_index: 0,
            completions_hidden: false,
            _subscriptions: Vec::new(),
        });
        self.sync_formula_highlights(slot, cx);
        cx.notify();
    }

    fn unused_column_name(&self, slot: usize) -> String {
        let taken = |n: &str| {
            self.result_sets
                .get(slot)
                .is_some_and(|rs| rs.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n)))
        };
        (1..)
            .map(|i| {
                if i == 1 {
                    crate::localization::text("results.formula_bar.default_computed_name")
                } else {
                    crate::localization::format(
                        "results.formula_bar.default_computed_name_numbered",
                        &[("column_number", &i.to_string())],
                    )
                }
            })
            .find(|n| !taken(n))
            .unwrap_or_else(|| {
                crate::localization::text("results.formula_bar.default_computed_name")
            })
    }

    pub fn close_formula_bar(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = self.formula_bar.take() {
            self.sync_formula_highlights(bar.slot, cx);
        }
        cx.notify();
    }

    /// Whether `name` is one of `slot`'s computed columns.
    pub(crate) fn is_computed_column(&self, slot: usize, name: &str) -> bool {
        self.computed_plans
            .get(slot)
            .and_then(|p| p.as_ref())
            .is_some_and(|plan| {
                plan.columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(name))
            })
    }

    /// Apply the bar: compute the column now, and ask the host to write the
    /// directive into the query.
    fn apply_formula_bar(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.formula_bar.as_mut() else {
            return;
        };
        let name = bar.name.trim().to_string();
        if name.is_empty() {
            bar.error = Some(crate::localization::text(
                "results.formula_bar.name_required",
            ));
            cx.notify();
            return;
        }
        if name.contains('=') || name.contains('\n') {
            bar.error = Some(crate::localization::text(
                "results.formula_bar.invalid_name",
            ));
            cx.notify();
            return;
        }
        let formula = match bar.parsed() {
            Ok(f) => f,
            Err(e) => {
                bar.error = Some(e.message);
                cx.notify();
                return;
            }
        };
        let source = bar
            .formula
            .trim()
            .trim_start_matches('=')
            .trim()
            .to_string();
        let line = crate::computed_columns::directive_line(&name, &source, bar.after.as_deref());
        if !crate::result_lock::is_block_safe(&line) {
            bar.error = Some(crate::localization::text(
                "results.formula_bar.comment_delimiters_forbidden",
            ));
            cx.notify();
            return;
        }
        let spec = ComputedColumn {
            name: name.clone(),
            formula: Formula {
                source: source.clone(),
                ..formula
            },
            after: bar.after.clone(),
            result: None,
        };
        let replacing = bar.replacing.clone();
        let slot = bar.slot;
        // Replace the edited spec in place (keeping its evaluation order),
        // or add the new one last.
        let at = replacing.as_ref().and_then(|old| {
            self.computed_specs
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(old))
        });
        match at {
            Some(i) => self.computed_specs[i] = spec,
            None => {
                self.computed_specs
                    .retain(|c| !c.name.eq_ignore_ascii_case(&name));
                self.computed_specs.push(spec);
            }
        }
        self.formula_bar = None;
        self.recompute_computed_columns(cx);
        self.sync_formula_highlights(slot, cx);
        cx.emit(ResultsViewEvent::ComputedColumnWritten { line, replacing });
        cx.notify();
    }

    /// Re-plan every in-memory result for the current computed columns and
    /// rebuild its grid: the source columns are recovered from the old plan,
    /// so nothing has to run again.
    pub(crate) fn recompute_computed_columns(&mut self, cx: &mut Context<Self>) {
        for slot in 0..self.result_sets.len() {
            if matches!(self.spill_backings.get(slot), Some(Some(_))) {
                self.messages.push(crate::localization::format(
                    "results.formula_bar.spill_recompute_notice",
                    &[("result_number", &(slot + 1).to_string())],
                ));
                continue;
            }
            while self.computed_plans.len() <= slot {
                self.computed_plans.push(None);
            }
            let old = self.computed_plans[slot].take();
            let rs = &mut self.result_sets[slot];
            let (source_columns, source_rows) = match &old {
                Some(plan) => strip_plan(plan, &rs.columns, std::mem::take(&mut rs.rows)),
                None => (rs.columns.clone(), std::mem::take(&mut rs.rows)),
            };
            let plan = ComputedPlan::new(&self.computed_specs, &source_columns, slot);
            let (columns, rows) = match &plan {
                Some(plan) => (
                    plan.output_columns(&source_columns),
                    plan.extend_rows(&[], source_rows, &self.formula_variables, self.formula_now),
                ),
                None => (source_columns, source_rows),
            };
            let rs = &mut self.result_sets[slot];
            rs.columns = columns;
            rs.rows = rows;
            rs.truncated.clear();
            self.computed_plans[slot] = plan;
            if let Some(view) = self.grid_views.get_mut(slot) {
                *view = None;
            }
        }
        self.ensure_grid_for_active_tab(cx);
        if self.layout_mode != super::LayoutMode::Tabbed {
            self.ensure_all_grids(cx);
        }
    }

    /// Outline the cells the formula being edited — or the selected computed
    /// cell's formula — reads, in its reference colors. Cheap and idempotent:
    /// the grid is only notified when the highlights change.
    pub(crate) fn sync_formula_highlights(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        let highlights = self.formula_highlights(slot, cx).unwrap_or_default();
        state.update(cx, |s, cx| {
            if *s.reference_highlights != highlights {
                s.set_reference_highlights(highlights);
                cx.notify();
            }
        });
    }

    /// The formula shown for `slot` right now: the bar's (while it parses),
    /// else the selected computed cell's.
    fn shown_formula(&self, slot: usize, cx: &App) -> Option<Formula> {
        if let Some(bar) = self.formula_bar.as_ref().filter(|b| b.slot == slot) {
            return bar.parsed().ok();
        }
        let name = self.selected_computed_column(slot, cx)?;
        self.computed_specs
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&name))
            .map(|c| c.formula.clone())
    }

    /// The computed column under the grid's selected cell.
    fn selected_computed_column(&self, slot: usize, cx: &App) -> Option<String> {
        let grid = self.grid_views.get(slot)?.as_ref()?;
        let state = grid.read(cx).state.read(cx);
        let sqlly_datatable::Selection::Cell(_, col) = state.selection else {
            return None;
        };
        let columns = &self.result_sets.get(slot)?.columns;
        let marker = grid_marker_width(&state.data.columns, columns.len());
        let name = &columns.get(col.checked_sub(marker)?)?.name;
        self.is_computed_column(slot, name).then(|| name.clone())
    }

    fn formula_highlights(&self, slot: usize, cx: &App) -> Option<ReferenceHighlights> {
        let formula = self.shown_formula(slot, cx)?;
        let columns = &self.result_sets.get(slot)?.columns;
        let grid = self.grid_views.get(slot)?.as_ref()?;
        let state = grid.read(cx).state.read(cx);
        let marker = grid_marker_width(&state.data.columns, columns.len());
        let windowed = state.window.is_some();
        // The row the formula is read for: the selected one, else the first.
        let current = state
            .selection
            .normalized_bounds()
            .filter(|(row, ..)| *row != usize::MAX)
            .and_then(|(row, ..)| state.resident_row_for_display(row))
            .unwrap_or(0);
        let map = self.source_index_map.get(slot).and_then(|m| m.clone());
        let to_result = |resident: usize| {
            map.as_ref()
                .map_or(Some(resident), |m| m.get(resident).copied())
        };
        let to_resident = |result: usize| match &map {
            Some(m) => m.iter().position(|r| *r == result),
            None => Some(result),
        };
        let current_result = to_result(current).unwrap_or(0) as i64;
        let column_of = |name: &str| {
            columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .map(|i| i + marker)
        };
        let colors = reference_colors(&formula.refs);
        let mut out = ReferenceHighlights {
            cells: HashMap::new(),
            columns: HashMap::new(),
        };
        let row_count = self.result_sets.get(slot)?.rows.len() as i64;
        let mut i = 0;
        while i < formula.refs.len() {
            let r = &formula.refs[i];
            let color = colors[i];
            // A range is two adjacent spans joined by ':'.
            let range_end = formula
                .refs
                .get(i + 1)
                .filter(|next| formula.source[r.end..next.start].trim() == ":");
            let (cols, rows) = match range_end {
                Some(end) => {
                    let (a, b) = (column_of(&r.column), column_of(&end.column));
                    let cols: Vec<usize> = match (a, b) {
                        (Some(a), Some(b)) => (a.min(b)..=a.max(b)).collect(),
                        _ => Vec::new(),
                    };
                    let ra = cell_row(r, current_result);
                    let rb = cell_row(end, current_result);
                    (cols, (ra.min(rb)..=ra.max(rb)).collect::<Vec<_>>())
                }
                None => (
                    column_of(&r.column).into_iter().collect(),
                    vec![cell_row(r, current_result)],
                ),
            };
            for col in &cols {
                out.columns.entry(*col).or_insert(color);
                if windowed {
                    continue;
                }
                for row in &rows {
                    if (0..row_count).contains(row)
                        && let Some(resident) = to_resident(*row as usize)
                    {
                        out.cells.entry((resident, *col)).or_insert(color);
                    }
                }
            }
            i += if range_end.is_some() { 2 } else { 1 };
        }
        Some(out)
    }

    /// The formula field's completion list at its caret, unless dismissed.
    fn formula_completions(&self, cx: &App) -> Option<(std::ops::Range<usize>, Vec<Suggestion>)> {
        let bar = self.formula_bar.as_ref()?;
        if bar.completions_hidden {
            return None;
        }
        let input = bar.formula_input.as_ref()?.read(cx);
        // Every column of the result except the one being edited.
        let columns: Vec<String> = self
            .result_sets
            .get(bar.slot)?
            .columns
            .iter()
            .map(|c| c.name.clone())
            .filter(|n| {
                !bar.replacing
                    .as_ref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(n))
            })
            .collect();
        let variables: Vec<String> = self
            .formula_variables
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        completions_at(&input.value(), input.cursor(), &columns, &variables)
    }

    /// Move the completion highlight by `delta`; false when no list is open.
    fn step_formula_completion(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some((_, items)) = self.formula_completions(cx) else {
            return false;
        };
        let Some(bar) = self.formula_bar.as_mut() else {
            return false;
        };
        let len = items.len() as isize;
        let at = bar.completion_index.min(items.len() - 1) as isize;
        bar.completion_index = (at + delta).rem_euclid(len) as usize;
        cx.notify();
        true
    }

    /// Insert completion `pick` (the highlighted one when `None`) over the
    /// typed prefix; false when no list is open.
    fn accept_formula_completion(
        &mut self,
        pick: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((range, items)) = self.formula_completions(cx) else {
            return false;
        };
        let Some(bar) = self.formula_bar.as_mut() else {
            return false;
        };
        let Some(input) = bar.formula_input.clone() else {
            return false;
        };
        let item = &items[pick.unwrap_or(bar.completion_index).min(items.len() - 1)];
        let text = input.read(cx).value().to_string();
        let new = format!(
            "{}{}{}",
            &text[..range.start],
            item.insert,
            &text[range.end..]
        );
        let caret = (new[..range.start + item.insert.len()]
            .encode_utf16()
            .count()) as u32;
        input.update(cx, |state, cx| {
            state.set_value(new.clone(), window, cx);
            state.set_cursor_position(gpui_component::input::Position::new(0, caret), window, cx);
            state.focus(window, cx);
        });
        bar.formula = new;
        bar.error = None;
        bar.completion_index = 0;
        let slot = bar.slot;
        self.sync_formula_highlights(slot, cx);
        cx.notify();
        true
    }

    fn render_formula_completions(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let s = Theme::scaled;
        let (_, items) = self.formula_completions(cx)?;
        let selected = self
            .formula_bar
            .as_ref()?
            .completion_index
            .min(items.len() - 1);
        let dismiss = cx.entity();
        Some(
            div()
                .absolute()
                .top(grown(26.0))
                .left(px(0.0))
                .child(
                    deferred(
                        anchored().snap_to_window_with_margin(px(s(8.0))).child(
                            div()
                                .occlude()
                                .min_w(px(s(240.0)))
                                .bg(Theme::popup_bg())
                                .border_1()
                                .border_color(Theme::popup_border())
                                .rounded(px(s(4.0)))
                                .p(px(s(4.0)))
                                .child(
                                    div()
                                        .id("formula-completions")
                                        .flex()
                                        .flex_col()
                                        .max_h(px(s(240.0)))
                                        .overflow_y_scroll()
                                        .children(items.into_iter().enumerate().map(
                                            |(i, item)| {
                                                let kind = match item.kind {
                                                    SuggestionKind::Function => "fx",
                                                    SuggestionKind::Column => "col",
                                                    SuggestionKind::Variable => "var",
                                                };
                                                div()
                                                    .id(("formula-completion", i))
                                                    .px(px(s(8.0)))
                                                    .h(px(s(22.0)))
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap(px(s(8.0)))
                                                    .font_family(Theme::editor_font_family())
                                                    .text_size(px(Theme::font_size()))
                                                    .text_color(Theme::text())
                                                    .when(i == selected, |d| {
                                                        d.bg(Theme::selection())
                                                    })
                                                    .hover(|st| st.bg(Theme::row_hover()))
                                                    .child(
                                                        div()
                                                            .w(px(s(24.0)))
                                                            .text_size(px(Theme::font_size_ratio(
                                                                0.8,
                                                            )))
                                                            .text_color(Theme::text_dim())
                                                            .child(kind),
                                                    )
                                                    .child(item.label)
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.accept_formula_completion(
                                                                Some(i),
                                                                window,
                                                                cx,
                                                            );
                                                        }),
                                                    )
                                            },
                                        )),
                                )
                                .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                                    dismiss.update(app, |this, cx| {
                                        if let Some(bar) = this.formula_bar.as_mut() {
                                            bar.completions_hidden = true;
                                        }
                                        cx.notify();
                                    });
                                }),
                        ),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    /// Materialize the bar's inputs (they need a `Window`) and render it, or
    /// the read-only formula of a selected computed cell.
    pub(super) fn render_formula_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let Some(bar) = self.formula_bar.as_mut() else {
            let slot = self.active_tab;
            let name = self.selected_computed_column(slot, cx)?;
            let formula = self.shown_formula(slot, cx)?;
            return Some(
                strip()
                    .child(fx_label())
                    .child(
                        div()
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "results.formula_bar.collapsed_column_name",
                                &[("column_name", &name)],
                            )),
                    )
                    .child(colored_formula(&formula))
                    .child(div().flex_1())
                    .child(
                        Button::new("formula-bar-edit")
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.formula_bar.edit_formula_button",
                            ))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.open_formula_bar(slot, Some(name.clone()), cx);
                            })),
                    )
                    .into_any_element(),
            );
        };
        if bar.name_input.is_none() {
            let name_input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "results.formula_bar.column_name_placeholder",
                    ),
                );
                state.set_value(bar.name.clone(), window, cx);
                state
            });
            let formula_input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "results.formula_bar.formula_placeholder",
                    ),
                );
                state.set_value(bar.formula.clone(), window, cx);
                state.focus(window, cx);
                state
            });
            let subs = vec![
                cx.subscribe_in(
                    &name_input,
                    window,
                    |this: &mut Self, input, event: &InputEvent, _w, cx| match event {
                        InputEvent::Change => {
                            if let Some(bar) = this.formula_bar.as_mut() {
                                bar.name = input.read(cx).value().to_string();
                                bar.error = None;
                            }
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.apply_formula_bar(cx),
                        _ => {}
                    },
                ),
                cx.subscribe_in(
                    &formula_input,
                    window,
                    |this: &mut Self, input, event: &InputEvent, _w, cx| match event {
                        InputEvent::Change => {
                            let slot = match this.formula_bar.as_mut() {
                                Some(bar) => {
                                    bar.formula = input.read(cx).value().to_string();
                                    bar.error = None;
                                    bar.completion_index = 0;
                                    bar.completions_hidden = false;
                                    bar.slot
                                }
                                None => return,
                            };
                            this.sync_formula_highlights(slot, cx);
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.apply_formula_bar(cx),
                        InputEvent::Blur => {
                            if let Some(bar) = this.formula_bar.as_mut() {
                                bar.completions_hidden = true;
                            }
                            cx.notify();
                        }
                        _ => {}
                    },
                ),
            ];
            bar.name_input = Some(name_input);
            bar.formula_input = Some(formula_input);
            bar._subscriptions = subs;
        }
        let name_input = bar.name_input.clone()?;
        let formula_input = bar.formula_input.clone()?;
        let parsed = bar.parsed();
        let status: gpui::AnyElement = match (&bar.error, &parsed) {
            (Some(error), _) => div()
                .text_color(Theme::error())
                .child(error.clone())
                .into_any_element(),
            (None, Ok(formula)) if !formula.refs.is_empty() => {
                colored_formula(formula).into_any_element()
            }
            (None, Ok(_)) => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.formula_bar.reference_guidance",
                ))
                .into_any_element(),
            (None, Err(e)) if bar.formula.trim().trim_start_matches('=').is_empty() => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.formula_bar.formula_guidance",
                ))
                .into_any_element(),
            (None, Err(e)) => div()
                .text_color(Theme::text_dim())
                .child(e.message.clone())
                .into_any_element(),
        };
        let editing = bar.replacing.is_some();
        let completions = self.render_formula_completions(cx);
        Some(
            div()
                .flex()
                .flex_col()
                .gap(grown(3.0))
                .px(grown(8.0))
                .py(grown(4.0))
                .text_size(px(Theme::font_size() * EDITOR_GROW))
                .bg(Theme::panel_bg())
                .border_b_1()
                .border_color(Theme::border())
                .on_key_down(
                    cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_formula_bar(cx);
                        }
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(grown(6.0))
                        .child(fx_label())
                        .child(div().w(grown(150.0)).child(grown_input(&name_input)))
                        .child(div().text_color(Theme::text_dim()).child("="))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
                                    if this.step_formula_completion(1, cx) {
                                        cx.stop_propagation();
                                    }
                                }))
                                .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
                                    if this.step_formula_completion(-1, cx) {
                                        cx.stop_propagation();
                                    }
                                }))
                                .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                                    if this.accept_formula_completion(None, window, cx) {
                                        cx.stop_propagation();
                                    }
                                }))
                                .capture_action(cx.listener(
                                    |this, _: &IndentInline, window, cx| {
                                        if this.accept_formula_completion(None, window, cx) {
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .capture_action(cx.listener(|this, _: &Escape, _window, cx| {
                                    if this.formula_completions(cx).is_some() {
                                        if let Some(bar) = this.formula_bar.as_mut() {
                                            bar.completions_hidden = true;
                                        }
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                }))
                                .child(grown_input(&formula_input))
                                .children(completions),
                        )
                        .child(
                            Button::new("formula-bar-apply")
                                .primary()
                                .small()
                                .label(crate::localization::text(if editing {
                                    "results.formula_bar.update_column_button"
                                } else {
                                    "results.formula_bar.add_column_button"
                                }))
                                .app_tooltip_text(crate::localization::text(
                                    "results.formula_bar.apply_tooltip",
                                ))
                                .on_click(
                                    cx.listener(|this, _, _window, cx| this.apply_formula_bar(cx)),
                                ),
                        )
                        .child(
                            Button::new("formula-bar-cancel")
                                .ghost()
                                .small()
                                .label(crate::localization::text(
                                    "results.formula_bar.cancel_button",
                                ))
                                .on_click(
                                    cx.listener(|this, _, _window, cx| this.close_formula_bar(cx)),
                                ),
                        ),
                )
                .child(
                    div()
                        .pl(grown(30.0))
                        .text_size(px(Theme::font_size_ratio(0.9) * EDITOR_GROW))
                        .child(status),
                )
                .into_any_element(),
        )
    }
}

/// The result row a reference reads from result row `current`.
fn cell_row(r: &RefSpan, current: i64) -> i64 {
    match r.row {
        formula::RowRef::Relative(back) => current - back,
        formula::RowRef::Absolute(n) => n - 1,
    }
}

/// `!Name`, quoted when the name needs it.
pub(crate) fn reference_text(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        format!("!{name}")
    } else {
        format!("!\"{}\"", name.replace('"', "\"\""))
    }
}

/// What a completion inserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuggestionKind {
    Function,
    Column,
    Variable,
}

/// One entry of the formula field's completion list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Suggestion {
    pub kind: SuggestionKind,
    pub label: String,
    /// Replaces the typed prefix.
    pub insert: String,
}

/// The completion list for `text` with the caret at byte `cursor`: the byte
/// range the chosen entry replaces and the entries. A `!` (or `!"`) prefix
/// lists columns, `@` lists variables, and a bare word outside a string
/// lists functions. `None` when nothing applies.
pub(crate) fn completions_at(
    text: &str,
    cursor: usize,
    columns: &[String],
    variables: &[String],
) -> Option<(std::ops::Range<usize>, Vec<Suggestion>)> {
    const LIMIT: usize = 12;
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    // A quoted reference still being typed: `!"Order T`.
    if let Some(bang) = before.rfind("!\"") {
        let typed = &before[bang + 2..];
        if !typed.contains('"') && quotes_balanced(&before[..bang]) {
            let items = rank(columns, typed, LIMIT)
                .into_iter()
                .map(|c| Suggestion {
                    kind: SuggestionKind::Column,
                    label: c.clone(),
                    insert: reference_text(c),
                })
                .collect::<Vec<_>>();
            return (!items.is_empty()).then_some((bang..cursor, items));
        }
    }
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(cursor, |(i, _)| i);
    let word = &before[start..];
    if !quotes_balanced(&before[..start]) {
        return None;
    }
    let sigil = before[..start].chars().next_back();
    let (range, items): (_, Vec<Suggestion>) = match sigil {
        Some('!') => (
            start - 1..cursor,
            rank(columns, word, LIMIT)
                .into_iter()
                .map(|c| Suggestion {
                    kind: SuggestionKind::Column,
                    label: c.clone(),
                    insert: reference_text(c),
                })
                .collect(),
        ),
        Some('@') => (
            start - 1..cursor,
            rank(variables, word, LIMIT)
                .into_iter()
                .map(|v| Suggestion {
                    kind: SuggestionKind::Variable,
                    label: format!("@{v}"),
                    insert: format!("@{v}"),
                })
                .collect(),
        ),
        _ if word.is_empty() || !word.starts_with(|c: char| c.is_alphabetic()) => return None,
        _ => {
            let paren_follows = text[cursor..].starts_with('(');
            let items = crate::formula::FUNCTIONS
                .iter()
                .filter(|f| f.len() >= word.len() && f[..word.len()].eq_ignore_ascii_case(word))
                .take(LIMIT)
                .map(|f| Suggestion {
                    kind: SuggestionKind::Function,
                    label: (*f).to_string(),
                    insert: if paren_follows {
                        (*f).to_string()
                    } else {
                        format!("{f}(")
                    },
                })
                .collect::<Vec<_>>();
            // A finished name already followed by its "(" needs no list.
            if paren_follows && items.len() == 1 && items[0].label.eq_ignore_ascii_case(word) {
                return None;
            }
            (start..cursor, items)
        }
    };
    // A finished reference or name needs no list (Enter applies the bar).
    let typed = &text[range.clone()];
    if items.is_empty() || items.iter().any(|s| s.insert.eq_ignore_ascii_case(typed)) {
        return None;
    }
    Some((range, items))
}

/// Whether `text` closes every string literal it opens.
fn quotes_balanced(text: &str) -> bool {
    text.matches('"').count().is_multiple_of(2)
}

/// `names` matching `typed`: prefix matches first, then other substring
/// matches, each in their original order.
fn rank<'a>(names: &'a [String], typed: &str, limit: usize) -> Vec<&'a String> {
    let typed = typed.to_lowercase();
    let (mut prefix, mut inner): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
    for n in names {
        let lower = n.to_lowercase();
        if lower.starts_with(&typed) {
            prefix.push(n);
        } else if lower.contains(&typed) {
            inner.push(n);
        }
    }
    prefix.extend(inner);
    prefix.truncate(limit);
    prefix
}

/// The source columns and rows behind a planned result (the computed
/// columns removed), so a changed set of formulas can be applied afresh.
fn strip_plan(
    plan: &ComputedPlan,
    columns: &[sqlly_core::ResultColumn],
    rows: Vec<Vec<sqlly_core::CellValue>>,
) -> (
    Vec<sqlly_core::ResultColumn>,
    Vec<Vec<sqlly_core::CellValue>>,
) {
    let mut order: Vec<(usize, usize)> = plan
        .layout
        .iter()
        .enumerate()
        .filter_map(|(pos, slot)| match slot {
            Slot::Source(i) => Some((*i, pos)),
            Slot::Computed(_) => None,
        })
        .collect();
    order.sort_unstable();
    let source_columns = order
        .iter()
        .filter_map(|(_, pos)| columns.get(*pos).cloned())
        .collect();
    let source_rows = rows
        .into_iter()
        .map(|row| {
            order
                .iter()
                .map(|(_, pos)| {
                    row.get(*pos)
                        .cloned()
                        .unwrap_or(sqlly_core::CellValue::Null)
                })
                .collect()
        })
        .collect();
    (source_columns, source_rows)
}

fn strip() -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(6.0)))
        .px(px(s(8.0)))
        .py(px(s(4.0)))
        .bg(Theme::panel_bg())
        .border_b_1()
        .border_color(Theme::border())
        .font_family(Theme::editor_font_family())
        .text_size(px(Theme::font_size()))
}

fn fx_label() -> gpui::Div {
    div()
        .font_weight(FontWeight::BOLD)
        .italic()
        .text_color(Theme::text_dim())
        .child("fx")
}

/// The formula text with every reference in its color, like Excel's
/// formula bar while editing.
fn colored_formula(formula: &Formula) -> StyledText {
    let colors = reference_colors(&formula.refs);
    let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = formula
        .refs
        .iter()
        .zip(colors)
        .map(|(r, color)| {
            (
                r.start..r.end,
                HighlightStyle {
                    color: Some(color),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            )
        })
        .collect();
    StyledText::new(formula.source.clone()).with_highlights(highlights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_references_share_a_color_and_new_ones_get_the_next() {
        let f = formula::parse("=!A+!B[1]+!A+!b[1]+!C").unwrap();
        let c = reference_colors(&f.refs);
        assert_eq!(c[0], c[2]);
        assert_eq!(c[1], c[3]);
        assert_ne!(c[0], c[1]);
        assert_ne!(c[1], c[4]);
    }

    #[test]
    fn reference_text_quotes_when_needed() {
        assert_eq!(reference_text("Total"), "!Total");
        assert_eq!(reference_text("Order Total"), "!\"Order Total\"");
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bang_lists_columns_and_inserts_a_reference() {
        let cols = names(&["Total", "Order Total", "Qty"]);
        let (range, items) = completions_at("=1+!to", 6, &cols, &[]).unwrap();
        assert_eq!(range, 3..6);
        assert_eq!(items[0].label, "Total");
        assert_eq!(items[1].insert, "!\"Order Total\"");
        let (_, all) = completions_at("=!", 2, &cols, &[]).unwrap();
        assert_eq!(all.len(), 3);
        let (range, quoted) = completions_at("=!\"Order T", 10, &cols, &[]).unwrap();
        assert_eq!(range, 1..10);
        assert_eq!(quoted[0].label, "Order Total");
    }

    #[test]
    fn bare_words_list_functions_but_not_inside_strings() {
        let (range, items) = completions_at("=IF(su", 6, &[], &[]).unwrap();
        assert_eq!(range, 4..6);
        assert!(items.iter().any(|s| s.label == "SUM" && s.insert == "SUM("));
        assert!(completions_at("=\"su", 4, &[], &[]).is_none());
        assert!(completions_at("=SUM(", 5, &[], &[]).is_none());
        assert!(completions_at("=12", 3, &[], &[]).is_none());
        let (_, items) = completions_at("=SU(1)", 3, &[], &[]).unwrap();
        assert!(items.iter().all(|s| !s.insert.ends_with('(')));
    }

    #[test]
    fn a_finished_reference_closes_the_list() {
        let cols = names(&["Total", "Totals"]);
        assert!(completions_at("=!Total", 7, &cols, &[]).is_none());
        assert!(completions_at("=!\"Order Total\"", 15, &names(&["Order Total"]), &[]).is_none());
        assert_eq!(completions_at("=!Tot", 5, &cols, &[]).unwrap().1.len(), 2);
    }

    #[test]
    fn at_lists_variables() {
        let vars = names(&["rate", "region"]);
        let (range, items) = completions_at("=!A*@ra", 7, &[], &vars).unwrap();
        assert_eq!(range, 4..7);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert, "@rate");
    }
}
