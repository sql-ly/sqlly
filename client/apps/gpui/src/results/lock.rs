//! The result lock for [`ResultsView`]: honoring the `grid:` / `width=`
//! directives when a grid is built, and gathering the directive lines that
//! reproduce every result grid exactly as it looks now (the lock button in
//! front of each result's name).
//!
//! The text edit itself is `crate::result_lock` (pure); the host applies it
//! to the editor because it owns the buffer and the executed span.

use std::collections::HashSet;

use gpui::{App, Context, Entity};
use sqlly_datatable::{GridState, GridTab};

use crate::grid_directive::GridDirective;
use crate::grid_sort::{SortKey, SortSpec};
use crate::query_format::{QueryColumnFormat, QueryFormatHint};

use super::{LayoutMode, ResultsView, ResultsViewEvent, grid_marker_width};

/// What a lock click produced: the directive lines for the block, and notes
/// about state that a directive cannot carry (shown in Messages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LockSnapshot {
    pub lines: Vec<String>,
    pub notes: Vec<String>,
}

impl ResultsView {
    /// The lock in front of result `slot`'s name: writes the results' whole
    /// on-screen state into the query. Accent-colored while the query
    /// carries a lock block.
    pub(super) fn render_lock_button(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::tooltip::AppTooltip as _;
        use gpui::{
            ElementId, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _,
            div, px,
        };
        let locked = self.query_is_locked();
        let tooltip: gpui::SharedString = if locked {
            crate::localization::text("results.lock.locked_tooltip").into()
        } else {
            crate::localization::text("results.lock.unlocked_tooltip").into()
        };
        div()
            .id(ElementId::NamedInteger("result-lock".into(), slot as u64))
            .debug_selector(move || format!("result-lock-{slot}"))
            .flex()
            .items_center()
            .mr(px(crate::theme::Theme::scaled(4.0)))
            .text_color(if locked {
                crate::theme::Theme::accent()
            } else {
                crate::theme::Theme::text_dim()
            })
            .hover(|st| st.text_color(crate::theme::Theme::text()))
            .child(crate::icons::lock())
            .app_tooltip_text(tooltip)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    cx.stop_propagation();
                    this.request_lock(slot, cx);
                }),
            )
            .into_any_element()
    }
}

/// Result-column index of `name` (exact spelling first, then ignoring case).
fn column_index(columns: &[sqlly_core::ResultColumn], name: &str) -> Option<usize> {
    columns.iter().position(|c| c.name == name).or_else(|| {
        columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    })
}

impl ResultsView {
    /// The `grid:` settings in force for `slot`, if any line names it.
    fn grid_directive_for_slot(&self, slot: usize) -> Option<GridDirective> {
        GridDirective::for_slot(&self.grid_directives, slot)
    }

    /// Before the grid's rows are built: a directive sort replaces the
    /// remembered one, so the rows are ordered for it from the start. Only on
    /// the run's first build of the slot — later rebuilds keep whatever the
    /// user has changed since.
    pub(super) fn preload_directive_sort(&mut self, slot: usize) {
        if self.grid_directives_applied.contains(&slot) {
            return;
        }
        let Some(spec) = self.directive_sort_spec(slot) else {
            return;
        };
        self.pad_sort_specs();
        if let Some(target) = self.sort_specs.get_mut(slot) {
            *target = spec;
        }
    }

    /// The directive's sort as a grid sort spec (grid columns: past the
    /// staged-edit marker the build about to run will show).
    fn directive_sort_spec(&self, slot: usize) -> Option<SortSpec> {
        let directive = self.grid_directive_for_slot(slot)?;
        if directive.sort.is_empty() {
            return None;
        }
        let columns = &self.result_sets.get(slot)?.columns;
        let marker = self.marker_offset(slot);
        let keys: Vec<SortKey> = directive
            .sort
            .iter()
            .filter_map(|(name, descending)| {
                Some(SortKey {
                    column: column_index(columns, name)? + marker,
                    descending: *descending,
                })
            })
            .collect();
        (!keys.is_empty()).then_some(SortSpec { keys })
    }

    /// After the remembered layout: apply the query's `width=` and `grid:`
    /// directives to the freshly built grid of `slot`, once per run. A
    /// directive wins over the remembered layout and leaves its columns out
    /// of auto-sizing (a width nobody measured is a width someone chose).
    pub(super) fn apply_grid_directives(
        &mut self,
        slot: usize,
        state: &Entity<GridState>,
        cx: &mut Context<Self>,
    ) {
        if !self.grid_directives_applied.insert(slot) {
            return;
        }
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let directive = self.grid_directive_for_slot(slot);
        let font_size = self.live_grid_metrics().font_size;
        let widths: Vec<Option<f32>> = columns
            .iter()
            .map(|c| {
                crate::query_format::width_dp_for_column(&c.name, &self.format_hints)
                    .map(|dp| crate::query_format::dp_to_px(dp, font_size))
            })
            .collect();
        if directive.is_none() && widths.iter().all(Option::is_none) {
            return;
        }
        let sort = self.directive_sort_spec(slot);
        if let Some(spec) = &sort {
            self.pad_sort_specs();
            self.sort_specs[slot] = spec.clone();
        }
        let primary = sort.as_ref().and_then(SortSpec::primary);
        state.update(cx, |s, cx| {
            let marker = grid_marker_width(&s.data.columns, columns.len());
            for (col, width) in widths.iter().enumerate() {
                if let (Some(width), Some(grid_col)) = (width, s.data.columns.get_mut(col + marker))
                {
                    grid_col.width = width.clamp(
                        crate::column_layout::MIN_COLUMN_WIDTH,
                        crate::column_layout::MAX_COLUMN_WIDTH,
                    );
                }
            }
            if let Some(directive) = &directive {
                apply_arrangement(s, directive, &columns, marker);
            }
            if primary.is_some() {
                s.sort = primary;
            }
            s.recompute();
            cx.notify();
        });
        // Foreign-key label choices ride the remembered layout the label
        // fetch reads.
        if let Some(directive) = directive.filter(|d| !d.labels.is_empty()) {
            while self.applied_layouts.len() <= slot {
                self.applied_layouts.push(None);
            }
            if let Some(layout) = self.applied_layouts[slot].as_mut() {
                for (name, display) in &directive.labels {
                    let Some(local) = column_index(&columns, name).map(|i| columns[i].name.clone())
                    else {
                        continue;
                    };
                    match layout.fk_labels.iter_mut().find(|(c, _)| *c == local) {
                        Some(entry) => entry.1 = display.clone(),
                        None => layout.fk_labels.push((local, display.clone())),
                    }
                }
            }
        }
        // Secondary sort keys need no rebuild: `preload_directive_sort` put
        // the spec in place before this build ordered its source rows.
    }

    /// The `@name` values this run's formulas read (from the host, which
    /// knows the parameter strip's values).
    pub fn set_formula_variables(&mut self, variables: Vec<(String, crate::formula::Value)>) {
        self.formula_variables = variables;
    }

    /// A result with computed columns spilled to disk: register its file so
    /// every reader produces the same columns.
    pub(super) fn register_computed_spill(&self, slot: usize, spill_file: &str) {
        if let Some(Some(plan)) = self.computed_plans.get(slot) {
            crate::computed_columns::register_spill(
                spill_file,
                crate::computed_columns::ComputedWindow {
                    plan: plan.clone(),
                    variables: self.formula_variables.clone(),
                    now: self.formula_now,
                },
            );
        }
    }

    /// Whether the query that produced these results carries a lock block,
    /// or the lock was just written (the lock icons are highlighted then).
    pub(crate) fn query_is_locked(&self) -> bool {
        self.locked_by_click
            || self
                .query_sql
                .as_deref()
                .is_some_and(|sql| !crate::sqlly_directive::lock_blocks(sql).is_empty())
    }

    /// The host wrote the lock into the query.
    pub fn note_locked(&mut self, cx: &mut Context<Self>) {
        self.locked_by_click = true;
        cx.notify();
    }

    /// The lock icon in front of result `slot`'s name was clicked: gather
    /// the directives that reproduce every result as it looks now and hand
    /// them to the host, which writes them into the query.
    pub fn request_lock(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.result_sets.get(slot).is_none() {
            return;
        }
        let snapshot = self.lock_snapshot(slot, cx);
        cx.emit(ResultsViewEvent::LockRequested(snapshot));
    }

    /// The directive lines for the lock block. `clicked` breaks ties when
    /// more than one result could own the pivot or chart directive.
    pub(crate) fn lock_snapshot(&self, clicked: usize, cx: &App) -> LockSnapshot {
        let mut out = LockSnapshot::default();
        let count = self.result_sets.len();
        let indexed = count > 1;
        let tabs: Vec<GridTab> = (0..count)
            .map(|slot| {
                self.grid_views
                    .get(slot)
                    .and_then(|v| v.as_ref())
                    .map(|v| v.read(cx).active_tab())
                    .unwrap_or(GridTab::Grid)
            })
            .collect();

        // View: the presentation open on the result it belongs to.
        let pivot_slot = self.lock_owner(clicked, &tabs, GridTab::Pivot, cx);
        let chart_slot = self.lock_owner(clicked, &tabs, GridTab::Chart, cx);
        let view = [clicked]
            .into_iter()
            .chain(0..count)
            .find_map(|slot| match tabs.get(slot) {
                Some(GridTab::Pivot) if pivot_slot == Some(slot) => Some("pivot"),
                Some(GridTab::Chart) if chart_slot == Some(slot) => Some("chart"),
                _ => None,
            });
        if let Some(view) = view {
            out.lines.push(format!("-- sqlly view: {view}"));
        }

        // Layout, focus, names, and stacked row allotments.
        if indexed {
            let layout = match self.layout_mode {
                LayoutMode::Tabbed => "tabbed",
                LayoutMode::Stacked => "stacked",
                LayoutMode::Canvas => "canvas",
            };
            out.lines.push(format!("-- sqlly layout: {layout}"));
            if self.layout_mode == LayoutMode::Tabbed {
                out.lines
                    .push(crate::result_header_actions::focus_directive_line(
                        self.active_tab,
                    ));
            } else if let Some(focus) = self.layout_directives.focus_slot() {
                out.lines
                    .push(crate::result_header_actions::focus_directive_line(focus));
            }
        }
        for (slot, name) in &self.result_set_names {
            if *slot < count {
                out.lines
                    .push(format!("-- sqlly name {}: {name}", slot + 1));
            }
        }
        if indexed && self.layout_mode == LayoutMode::Stacked {
            for slot in 0..count {
                let directive_rows = self
                    .layout_directives
                    .lines
                    .iter()
                    .find(|(index, _)| *index == slot + 1)
                    .map(|(_, rows)| *rows);
                let resized = self.stacked_card_heights.get(slot).copied().flatten();
                // A dragged divider is what is on screen; otherwise the
                // allotment the query already makes.
                let rows = if resized.is_some() {
                    Some(self.visible_row_count_for_slot(slot, cx).max(1))
                } else {
                    directive_rows
                };
                if let Some(rows) = rows {
                    out.lines
                        .push(crate::result_header_actions::lines_directive_line(
                            slot, rows,
                        ));
                }
            }
        }

        // Computed columns, as declared (they come first: later lines may
        // format or arrange them by name).
        for spec in &self.computed_specs {
            let mut line = crate::computed_columns::directive_line(
                &spec.name,
                &spec.formula.source,
                spec.after.as_deref(),
            );
            if let Some(result) = spec.result {
                line.push_str(&format!("; result={result}"));
            }
            out.lines.push(line);
        }

        // Grid arrangement, one line per result.
        for slot in 0..count {
            if let Some(line) = self.lock_grid_line(slot, indexed, &mut out.notes, cx) {
                out.lines.push(line);
            }
        }

        // Column formats and widths (by column name, first result wins).
        let mut seen = HashSet::new();
        for slot in 0..count {
            for hint in self.lock_format_hints(slot, cx) {
                if seen.insert(hint.column.to_ascii_lowercase())
                    && let Some(line) = crate::query_format::directive_line(&hint)
                {
                    out.lines.push(line);
                }
            }
        }

        // Conditional formatting: this query's rules, then the saved rules
        // for any column the results have — the order they apply in.
        let names: HashSet<String> = self
            .result_sets
            .iter()
            .flat_map(|rs| rs.columns.iter().map(|c| c.name.to_ascii_lowercase()))
            .collect();
        let mut rules = self.conditional_directives.clone();
        for rule in crate::conditional_formatting::load_rules() {
            if names.contains(&rule.column.to_ascii_lowercase()) && !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        for rule in &rules {
            match crate::conditional_directive::directive_line(rule) {
                Some(line) => out.lines.push(line),
                None => out.notes.push(crate::localization::format(
                    "results.lock.conditional_rule_not_serializable",
                    &[("column_name", &rule.column)],
                )),
            }
        }

        // Pivot and chart configuration.
        if let Some(slot) = pivot_slot
            && let Some(line) = self.lock_pivot_line(slot, cx)
        {
            out.lines.push(line);
        }
        if let Some(slot) = chart_slot
            && let Some(line) = self.lock_chart_line(slot, cx)
        {
            out.lines.push(line);
        }

        // Map settings live in the map window, not the grid: carry the
        // query's map lines into the rewritten block unchanged.
        let map_lines = crate::map_view::directive::carry_over_lines(self.query_sql.as_deref());
        out.lines.extend(map_lines);

        // A line that would end the block comment early can't go inside it.
        out.lines.retain(|line| {
            let safe = crate::result_lock::is_block_safe(line);
            if !safe {
                out.notes.push(crate::localization::format(
                    "results.lock.unsafe_comment_line_omitted",
                    &[("line", line)],
                ));
            }
            safe
        });
        if self
            .client_filter
            .iter()
            .take(count)
            .any(|filter| filter.is_some())
        {
            out.notes.push(crate::localization::text(
                "results.lock.filter_builder_not_locked",
            ));
        }
        out
    }

    /// The result that owns the lock's pivot (or chart) directive: one
    /// showing that tab (the clicked result first), else one already
    /// configured by a directive or the sidebar.
    fn lock_owner(
        &self,
        clicked: usize,
        tabs: &[GridTab],
        tab: GridTab,
        cx: &App,
    ) -> Option<usize> {
        let count = self.result_sets.len();
        let order = || {
            std::iter::once(clicked)
                .chain(0..count)
                .filter(move |s| *s < count)
        };
        if let Some(slot) = order().find(|slot| tabs.get(*slot) == Some(&tab)) {
            return Some(slot);
        }
        order().find(|slot| match tab {
            GridTab::Pivot => {
                self.pivot_config_for_slot(*slot).is_some()
                    || self
                        .live_pivot_config(*slot, cx)
                        .is_some_and(|c| c != sqlly_datatable::PivotConfig::default())
            }
            GridTab::Chart => {
                self.chart_configs.contains_key(slot)
                    || self.chart_directive_config(*slot).is_some()
            }
            _ => false,
        })
    }

    fn live_pivot_config(&self, slot: usize, cx: &App) -> Option<sqlly_datatable::PivotConfig> {
        let view = self.grid_views.get(slot)?.as_ref()?.read(cx);
        Some(view.pivot_state()?.read(cx).config.clone())
    }

    fn lock_pivot_line(&self, slot: usize, cx: &App) -> Option<String> {
        let config = self
            .live_pivot_config(slot, cx)
            .or_else(|| self.pivot_config_for_slot(slot))?;
        let columns = &self.result_sets.get(slot)?.columns;
        crate::result_pivot::ResultPivotSpec::from_config(&config, columns, slot)
            .map(|spec| spec.directive())
    }

    fn lock_chart_line(&self, slot: usize, cx: &App) -> Option<String> {
        let live = self
            .grid_views
            .get(slot)
            .and_then(|v| v.as_ref())
            .and_then(|v| v.read(cx).chart_state().map(|s| s.read(cx).config.clone()));
        let config = live.or_else(|| self.chart_config_for_slot(slot))?;
        let columns = &self.result_sets.get(slot)?.columns;
        crate::result_chart::ResultChartSpec::from_config(&config, columns, slot)
            .map(|spec| spec.directive())
    }

    /// The `grid` line for `slot`: its display order, hidden and frozen
    /// columns, sort, grouping, column filters, and foreign-key labels.
    fn lock_grid_line(
        &self,
        slot: usize,
        indexed: bool,
        notes: &mut Vec<String>,
        cx: &App,
    ) -> Option<String> {
        let columns = &self.result_sets.get(slot)?.columns;
        let grid = self.grid_views.get(slot)?.as_ref()?.read(cx);
        let state = grid.state.read(cx);
        let marker = grid_marker_width(&state.data.columns, columns.len());
        let name = |grid_col: usize| -> Option<String> {
            columns
                .get(grid_col.checked_sub(marker)?)
                .map(|c| c.name.clone())
        };
        let mut directive = GridDirective {
            index: indexed.then_some(slot + 1),
            ..GridDirective::default()
        };
        let order: Vec<usize> = state.column_order().to_vec();
        let natural: Vec<usize> = (0..state.data.columns.len())
            .filter(|c| order.contains(c))
            .collect();
        if order != natural {
            directive.order = order.iter().filter_map(|c| name(*c)).collect();
        }
        directive.hide = state
            .hidden_columns()
            .into_iter()
            .filter_map(name)
            .collect();
        let frozen = state.frozen_columns().saturating_sub(marker);
        if frozen > 0 {
            directive.freeze = Some(frozen);
        }
        directive.sort = self
            .sort_spec_for_slot(slot)
            .keys
            .iter()
            .filter_map(|key| Some((name(key.column)?, key.descending)))
            .collect();
        if directive.sort.is_empty()
            && let Some((col, dir)) = state.sort
            && let Some(col_name) = name(col)
        {
            directive.sort = vec![(col_name, dir == sqlly_datatable::SortDirection::Descending)];
        }
        directive.group = state.grouped_column().and_then(name);
        for (grid_col, filter) in state.filters.iter().enumerate() {
            if filter.is_active()
                && let Some(col_name) = name(grid_col)
            {
                directive.filters.push((col_name, filter.clone()));
            }
        }
        if let Some(layout) = self.applied_layouts.get(slot).and_then(|l| l.as_ref()) {
            directive.labels = layout.fk_labels.clone();
        }
        let _ = notes;
        crate::grid_directive::directive_line(&directive)
    }

    /// The format directives for `slot`'s columns as they look now: the
    /// query's own `format:` rule, a Rounding choice made in the header
    /// menu, and every column's current width in dp.
    fn lock_format_hints(&self, slot: usize, cx: &App) -> Vec<QueryFormatHint> {
        let Some(columns) = self.result_sets.get(slot).map(|rs| &rs.columns) else {
            return Vec::new();
        };
        let Some(grid) = self.grid_views.get(slot).and_then(|v| v.as_ref()) else {
            return Vec::new();
        };
        let state = grid.read(cx).state.read(cx);
        let marker = grid_marker_width(&state.data.columns, columns.len());
        let baseline = self.grid_config_for_columns(slot, columns);
        let font_size = self.live_grid_metrics().font_size;
        columns
            .iter()
            .enumerate()
            .map(|(col, column)| {
                let mut format = self
                    .format_hints
                    .iter()
                    .find(|h| h.column.eq_ignore_ascii_case(&column.name))
                    .map(|h| h.format.clone())
                    .unwrap_or(QueryColumnFormat::Keep);
                // Rounding edits the live override's decimals in place.
                let live = state
                    .config
                    .column_overrides
                    .get(col + marker)
                    .and_then(|o| o.number);
                let built = baseline.column_overrides.get(col).and_then(|o| o.number);
                if let Some(live) = live
                    && built.is_none_or(|b| b.decimals != live.decimals)
                    && matches!(
                        format,
                        QueryColumnFormat::Keep | QueryColumnFormat::Number { .. }
                    )
                {
                    format = QueryColumnFormat::Number {
                        decimals: live.decimals,
                        thousands: live.thousands_separator,
                    };
                }
                let width = state
                    .data
                    .columns
                    .get(col + marker)
                    .map(|c| crate::query_format::px_to_dp(c.width, font_size));
                QueryFormatHint {
                    column: column.name.clone(),
                    format,
                    width_dp: width,
                    zone: crate::query_format::directive_zone_for(&column.name, &self.format_hints),
                }
            })
            .collect()
    }
}

/// Order, hide, freeze, group, and filter `state` per `directive`. Grid
/// columns are result columns shifted past `marker` leading chrome columns.
fn apply_arrangement(
    state: &mut GridState,
    directive: &GridDirective,
    columns: &[sqlly_core::ResultColumn],
    marker: usize,
) {
    let resolve = |name: &String| column_index(columns, name);
    if !directive.order.is_empty() || !directive.hide.is_empty() {
        let hidden: HashSet<usize> = directive.hide.iter().filter_map(resolve).collect();
        let mut order: Vec<usize> = Vec::new();
        for col in directive.order.iter().filter_map(resolve) {
            if !order.contains(&col) {
                order.push(col);
            }
        }
        for col in 0..columns.len() {
            if !order.contains(&col) {
                order.push(col);
            }
        }
        let display: Vec<usize> = (0..marker)
            .chain(
                order
                    .into_iter()
                    .filter(|c| !hidden.contains(c))
                    .map(|c| c + marker),
            )
            .collect();
        if display.len() > marker {
            state.set_column_order(&display);
        }
    }
    if let Some(freeze) = directive.freeze {
        state.set_frozen_columns(if freeze > 0 { freeze + marker } else { 0 });
    }
    if let Some(group) = directive.group.as_ref().and_then(resolve) {
        state.set_grouped_column(Some(group + marker));
    }
    for (name, filter) in &directive.filters {
        if let Some(col) = resolve(name)
            && let Some(slot) = state.filters.get_mut(col + marker)
        {
            *slot = filter.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<sqlly_core::ResultColumn> {
        names
            .iter()
            .map(|n| sqlly_core::ResultColumn {
                name: n.to_string(),
                data_type: "int".into(),
                nullable: true,
            })
            .collect()
    }

    #[test]
    fn column_names_resolve_exactly_then_ignoring_case() {
        let columns = cols(&["id", "ID", "Name"]);
        assert_eq!(column_index(&columns, "ID"), Some(1));
        assert_eq!(column_index(&columns, "name"), Some(2));
        assert_eq!(column_index(&columns, "missing"), None);
    }
}
